//! The Inbox model: the pure "what needs me now?" aggregation.
//!
//! [`ClientState`](crate::state::ClientState) stores the raw `tasks.list` rows
//! and the flattened `agents.list` sessions; this module turns them into the two
//! groups the surface renders (docs/design/gpui-client.md §1, Miller's Law):
//!
//! - **Needs you**: sessions blocked on the user plus failed tasks from the last
//!   [`FAILED_WINDOW_HOURS`] hours, oldest waiting first (a fair queue).
//! - **Recent**: the newest tasks, one line each, newest first.
//!
//! It is deliberately free of GPUI types so sorting, status derivation and
//! formatting are unit-tested without a window (docs §4, "Tests").

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use favetto_wire::model::{
    AgentCost, AgentSessionInfo, AwaitingInputKind, AwaitingInputReason, Task, TaskStatus,
};
use uuid::Uuid;

use crate::launch;
use crate::repos::repo_visible;

/// How far back a failure still counts as "needs you".
pub const FAILED_WINDOW_HOURS: i64 = 24;

/// How many rows the Recent group shows.
pub const RECENT_LIMIT: usize = 20;

/// The `repo:<name>/<path>` namespace prefix used by the daemon (Jakob's Law:
/// the desktop client spells tasks the same way the TUI and web do).
const REPO_PREFIX: &str = "repo:";

/// A task/session status as the UI renders it.
///
/// Derivation is deliberately string-based: an unrecognised value from a newer
/// daemon degrades to [`StatusKind::Neutral`] instead of hiding the row or
/// failing the whole list (docs §2, Postel's Law).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusKind {
    /// Queued, not yet picked up.
    Pending,
    /// Currently executing.
    Running,
    /// Blocked on the user.
    AwaitingInput,
    /// Completed successfully.
    Succeeded,
    /// Terminated with an error.
    Failed,
    /// Cancelled before completion.
    Cancelled,
    /// A status this client does not know; rendered without an alarm color.
    Neutral,
}

impl StatusKind {
    /// Derive the display status from the daemon's stable status string.
    pub fn from_label(label: Option<&str>) -> Self {
        match label {
            Some("pending") => Self::Pending,
            Some("running") => Self::Running,
            Some("awaiting_input") => Self::AwaitingInput,
            Some("succeeded") => Self::Succeeded,
            Some("failed") => Self::Failed,
            Some("cancelled") => Self::Cancelled,
            _ => Self::Neutral,
        }
    }

    /// The display status for a typed task status.
    pub fn from_task_status(status: TaskStatus) -> Self {
        Self::from_label(Some(status.as_str()))
    }

    /// The human label. Status is never conveyed by color alone (docs §3).
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::AwaitingInput => "awaiting input",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Neutral => "unknown",
        }
    }

    /// The glyph that accompanies the label.
    pub fn icon(self) -> &'static str {
        match self {
            Self::Pending => "◔",
            Self::Running => "◐",
            Self::AwaitingInput => "⚠",
            Self::Succeeded => "✓",
            Self::Failed => "✕",
            Self::Cancelled => "⊘",
            Self::Neutral => "?",
        }
    }
}

/// The stable identity of a Needs you row: a task, or a live session that is
/// blocked but has no task row in the current `tasks.list` window.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InboxKey {
    /// A task row.
    Task(Uuid),
    /// A session-only row (no task in the fetched window).
    Session(String),
}

impl InboxKey {
    /// The task id, when this row is backed by a task.
    pub fn task_id(&self) -> Option<Uuid> {
        match self {
            Self::Task(id) => Some(*id),
            Self::Session(_) => None,
        }
    }
}

/// One row of the Needs you group.
#[derive(Clone, Debug, PartialEq)]
pub struct NeedsRow {
    /// Stable identity used for selection across refreshes.
    pub key: InboxKey,
    /// Awaiting input or failed.
    pub status: StatusKind,
    /// The task name with the `repo:<name>/` prefix stripped.
    pub task_name: String,
    /// The agent entry that owns the session, when the daemon reports one.
    pub agent: Option<String>,
    /// Execution attempt, when the row is backed by a task.
    pub attempt: Option<u32>,
    /// When the row started needing a decision (fair-queue order and elapsed).
    pub since: DateTime<Utc>,
    /// The permission prompt (awaiting rows) or the failure message (failed).
    pub detail: Option<String>,
    /// True when [`NeedsRow::detail`] is a permission request message.
    pub detail_is_permission: bool,
}

/// One row of the Recent group.
#[derive(Clone, Debug, PartialEq)]
pub struct RecentRow {
    /// The task id.
    pub task_id: Uuid,
    /// The task's current status.
    pub status: StatusKind,
    /// The task name with the repo prefix stripped.
    pub name: String,
    /// The repo namespace, when the task is repo-local.
    pub repo: Option<String>,
    /// Elapsed time for a live run, total time for a finished one.
    pub duration: Option<Duration>,
    /// Usage cost reported by the task's live agent session, when known. The
    /// amount keeps its reported unit so a credits value is never shown as USD.
    pub cost: Option<AgentCost>,
    /// Creation time, used for the newest-first order.
    pub created_at: DateTime<Utc>,
}

/// Both groups, already sorted.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InboxRows {
    /// Oldest waiting first.
    pub needs: Vec<NeedsRow>,
    /// Newest first, capped at [`RECENT_LIMIT`].
    pub recent: Vec<RecentRow>,
}

impl InboxRows {
    /// True when the daemon returned nothing to show.
    pub fn is_empty(&self) -> bool {
        self.needs.is_empty() && self.recent.is_empty()
    }

    /// The Inbox badge count (blocked + recently failed).
    pub fn needs_count(&self) -> u32 {
        u32::try_from(self.needs.len()).unwrap_or(u32::MAX)
    }

    /// Every selectable row in visual order (Needs you first, then Recent).
    ///
    /// Recent rows are always task-backed, so they synthesize [`InboxKey::Task`]
    /// and participate in the same `↑`/`↓`/`Enter` selection as the top group.
    pub fn keys(&self) -> Vec<InboxKey> {
        self.needs
            .iter()
            .map(|row| row.key.clone())
            .chain(self.recent.iter().map(|row| InboxKey::Task(row.task_id)))
            .collect()
    }
}

/// The trimmed prompt text of an awaiting request, if the agent sent one.
fn request_message(request: &AwaitingInputReason) -> Option<String> {
    let message = request.message.trim();
    (!message.is_empty()).then(|| message.to_string())
}

/// Build both groups from a `tasks.list` snapshot and the flattened
/// `agents.list` sessions, scoped to `active_repo`.
///
/// An awaiting session is authoritative even when its task row still says
/// `running` (the status push can lag). A session with `task_id: None` (or an id
/// outside the fetched task window) still appears in Needs you, labelled
/// neutrally, so a blocked agent is never hidden just because its task row is
/// missing. With an active repo, rows whose repo cannot be resolved are treated
/// as global and hidden, matching the TUI's task/event scoping.
pub fn build(
    tasks: &[Task],
    sessions: &[AgentSessionInfo],
    active_repo: Option<&str>,
    now: DateTime<Utc>,
) -> InboxRows {
    let mut session_by_task: HashMap<&str, &AgentSessionInfo> = HashMap::new();
    let mut awaiting_by_task: HashMap<&str, &AgentSessionInfo> = HashMap::new();
    for session in sessions {
        let Some(task_id) = session.task_id.as_deref() else {
            continue;
        };
        session_by_task.entry(task_id).or_insert(session);
        if session.awaiting_input.is_some() {
            awaiting_by_task.entry(task_id).or_insert(session);
        }
    }

    let cutoff = now - chrono::Duration::hours(FAILED_WINDOW_HOURS);
    let launch_by_task: HashMap<Uuid, launch::LaunchFailure> = launch::launch_failures(tasks)
        .into_iter()
        .map(|failure| (failure.task_id, failure))
        .collect();
    let mut needs = Vec::new();
    let mut needs_task_ids = HashSet::new();

    for task in tasks {
        if !repo_visible(active_repo, task_repo(&task.name)) {
            continue;
        }
        let task_id = task.id.to_string();
        let agent = session_by_task
            .get(task_id.as_str())
            .map(|session| session.agent.clone());
        match task.status {
            TaskStatus::AwaitingInput => {
                let request = awaiting_by_task
                    .get(task_id.as_str())
                    .and_then(|session| session.awaiting_input.as_ref());
                needs.push(NeedsRow {
                    key: InboxKey::Task(task.id),
                    status: StatusKind::from_task_status(task.status),
                    task_name: task_display_name(&task.name).to_string(),
                    agent,
                    attempt: Some(task.attempt),
                    since: task.started_at.unwrap_or(task.created_at),
                    detail: request.and_then(request_message),
                    detail_is_permission: request
                        .is_some_and(|request| request.kind == AwaitingInputKind::Permission),
                });
                needs_task_ids.insert(task.id);
            }
            TaskStatus::Failed => {
                let since = task.finished_at.unwrap_or(task.created_at);
                let launch_failure = launch_by_task.get(&task.id);
                // A launch failure is a broken workflow, not a transient run
                // failure: keep it in Needs you regardless of the 24h window.
                if launch_failure.is_some() || since >= cutoff {
                    needs.push(NeedsRow {
                        key: InboxKey::Task(task.id),
                        status: StatusKind::Failed,
                        // The full `repo:<namespace>/<task>` target keeps a
                        // casing mismatch visible; display names strip it.
                        task_name: launch_failure
                            .map(|failure| failure.target.clone())
                            .unwrap_or_else(|| task_display_name(&task.name).to_string()),
                        agent,
                        attempt: Some(task.attempt),
                        since,
                        detail: launch_failure
                            .map(launch::LaunchFailure::detail)
                            .or_else(|| {
                                task.error
                                    .as_ref()
                                    .map(|error| error.trim().to_string())
                                    .filter(|error| !error.is_empty())
                            }),
                        detail_is_permission: false,
                    });
                    needs_task_ids.insert(task.id);
                }
            }
            _ => {}
        }
    }

    // A blocked session is the authoritative "a human is needed" signal —
    // including when its task row still says `running` because the status push
    // lags. A session-only row (no task in the fetched window) is still a
    // person-shaped obligation, so it appears with neutral identity fields.
    for session in sessions {
        let Some(request) = session.awaiting_input.as_ref() else {
            continue;
        };
        let task = session
            .task_id
            .as_deref()
            .and_then(|task_id| task_id.parse::<Uuid>().ok())
            .and_then(|task_id| tasks.iter().find(|task| task.id == task_id));

        let Some(task) = task else {
            // A session with no task row has no repo namespace; under an active
            // repo it is treated as global and hidden, like the TUI.
            if active_repo.is_some() {
                continue;
            }
            needs.push(NeedsRow {
                key: InboxKey::Session(session.id.clone()),
                status: StatusKind::AwaitingInput,
                task_name: "Unknown task".to_string(),
                agent: Some(session.agent.clone()),
                attempt: None,
                // No session start time is on the wire; use now so the row
                // sorts after rows whose wait time is known.
                since: now,
                detail: request_message(request),
                detail_is_permission: request.kind == AwaitingInputKind::Permission,
            });
            continue;
        };

        if !repo_visible(active_repo, task_repo(&task.name)) {
            continue;
        }

        // A stale session flag must not resurrect a finished task.
        if matches!(
            task.status,
            TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled
        ) {
            continue;
        }

        if needs_task_ids.contains(&task.id) {
            // The task loop already added the row (status `awaiting_input`);
            // attach the live prompt text to that same row.
            if let Some(row) = needs
                .iter_mut()
                .find(|row| row.key == InboxKey::Task(task.id))
            {
                row.detail = request_message(request);
                row.detail_is_permission = request.kind == AwaitingInputKind::Permission;
                if row.agent.is_none() {
                    row.agent = Some(session.agent.clone());
                }
            }
            continue;
        }

        needs.push(NeedsRow {
            key: InboxKey::Task(task.id),
            status: StatusKind::AwaitingInput,
            task_name: task_display_name(&task.name).to_string(),
            agent: Some(session.agent.clone()),
            attempt: Some(task.attempt),
            since: task.started_at.unwrap_or(task.created_at),
            detail: request_message(request),
            detail_is_permission: request.kind == AwaitingInputKind::Permission,
        });
        needs_task_ids.insert(task.id);
    }

    needs.sort_by(|left, right| {
        left.since
            .cmp(&right.since)
            .then_with(|| left.key.cmp(&right.key))
    });

    let mut costs: HashMap<&str, AgentCost> = HashMap::new();
    for session in sessions {
        let (Some(task_id), Some(usage)) = (session.task_id.as_deref(), session.usage.as_ref())
        else {
            continue;
        };
        if let Some(cost) = usage.reported_cost().filter(|cost| cost.amount.is_finite()) {
            match costs.entry(task_id) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    entry.get_mut().merge(&cost);
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(cost);
                }
            }
        }
    }

    let mut recent: Vec<RecentRow> = tasks
        .iter()
        .filter(|task| repo_visible(active_repo, task_repo(&task.name)))
        .filter(|task| !needs_task_ids.contains(&task.id))
        .map(|task| RecentRow {
            task_id: task.id,
            status: StatusKind::from_task_status(task.status),
            name: task_display_name(&task.name).to_string(),
            repo: task_repo(&task.name).map(str::to_string),
            duration: task_duration(task, now),
            cost: costs.get(task.id.to_string().as_str()).cloned(),
            created_at: task.created_at,
        })
        .collect();
    recent.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| right.task_id.cmp(&left.task_id))
    });
    recent.truncate(RECENT_LIMIT);

    InboxRows { needs, recent }
}

/// The repo namespace of a `repo:<name>/<path>` task name.
pub fn task_repo(name: &str) -> Option<&str> {
    let rest = name.strip_prefix(REPO_PREFIX)?;
    let (repo, path) = rest.split_once('/')?;
    if repo.is_empty() || path.is_empty() {
        None
    } else {
        Some(repo)
    }
}

/// The display name: strip only a well-formed repo prefix, so a malformed name
/// is never truncated (`repo:favetto/triage` → `triage`).
pub fn task_display_name(name: &str) -> &str {
    match task_repo(name) {
        Some(_) => name
            .strip_prefix(REPO_PREFIX)
            .and_then(|rest| rest.split_once('/'))
            .map(|(_, path)| path)
            .unwrap_or(name),
        None => name,
    }
}

/// Elapsed/total time in the compact form used by list rows.
pub fn format_elapsed(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h {:02}m", seconds / 3_600, (seconds % 3_600) / 60)
    } else {
        format!("{}d {}h", seconds / 86_400, (seconds % 86_400) / 3_600)
    }
}

/// Time since `since`, in the compact list form.
pub fn elapsed_since(since: DateTime<Utc>, now: DateTime<Utc>) -> String {
    format_elapsed(now.timestamp().saturating_sub(since.timestamp()))
}

/// A duration cell value; `—` when the daemon has no timestamps.
pub fn format_duration(duration: Option<Duration>) -> String {
    match duration {
        Some(duration) => format_elapsed(i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)),
        None => "—".to_string(),
    }
}

/// A usage-cost cell value with an explicit unit (`$0.0123`, `2.5000 credits`);
/// `None` when the cost is unknown or not finite.
pub fn format_cost(cost: Option<&AgentCost>) -> Option<String> {
    let cost = cost.filter(|cost| cost.amount.is_finite())?;
    let mut cost = cost.clone();
    cost.amount = cost.amount.max(0.0);
    Some(cost.display())
}

/// Elapsed time of a task: live elapsed for a running task, total for a
/// finished one, `None` before it starts.
pub fn task_duration(task: &Task, now: DateTime<Utc>) -> Option<Duration> {
    let started = task.started_at?;
    let end = task.finished_at.unwrap_or(now);
    let seconds = end.timestamp().saturating_sub(started.timestamp());
    Some(Duration::from_secs(u64::try_from(seconds).unwrap_or(0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use favetto_wire::model::{AgentUsage, AwaitingInputReason, Failure, FailureKind};

    fn at(offset_minutes: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp")
            + chrono::Duration::minutes(offset_minutes)
    }

    fn task(id: u128, name: &str, status: TaskStatus, created: DateTime<Utc>) -> Task {
        Task {
            id: Uuid::from_u128(id),
            name: name.to_string(),
            status,
            attempt: 1,
            input: serde_json::Value::Null,
            output: None,
            dedupe_key: None,
            created_at: created,
            started_at: None,
            finished_at: None,
            error: None,
            failure: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
            interactive: false,
        }
    }

    fn session(
        id: &str,
        task_id: Option<Uuid>,
        agent: &str,
        awaiting: Option<AwaitingInputReason>,
    ) -> AgentSessionInfo {
        AgentSessionInfo {
            id: id.to_string(),
            agent: agent.to_string(),
            backend: Default::default(),
            task_id: task_id.map(|id| id.to_string()),
            running: true,
            headless: true,
            session_id: None,
            awaiting_input: awaiting,
            activity: None,
            usage: None,
            load_state: Default::default(),
            load_error: None,
            config_options: Vec::new(),
            modes: None,
            auth: None,
        }
    }

    fn permission(message: &str) -> AwaitingInputReason {
        AwaitingInputReason {
            kind: AwaitingInputKind::Permission,
            message: message.to_string(),
            request_id: Some("req-1".to_string()),
            options: vec!["Allow once".to_string(), "Reject".to_string()],
            allow_always: false,
        }
    }

    #[test]
    fn blocked_sessions_and_recent_failures_are_needs_you_oldest_first() {
        let now = at(0);
        let awaiting_id = Uuid::from_u128(1);
        let mut awaiting = task(
            1,
            "repo:favetto/triage",
            TaskStatus::AwaitingInput,
            at(-180),
        );
        awaiting.started_at = Some(at(-120));
        let mut failed = task(2, "repo:favetto/test", TaskStatus::Failed, at(-90));
        failed.started_at = Some(at(-60));
        failed.finished_at = Some(at(-30));
        failed.error = Some("exit code 1".to_string());
        let sessions = vec![session(
            "s1",
            Some(awaiting_id),
            "opencode",
            Some(permission("Approve `git push`?")),
        )];

        let rows = build(&[awaiting, failed], &sessions, None, now);
        assert_eq!(rows.needs.len(), 2);
        // The awaiting row started waiting 120m ago, the failure 30m ago.
        assert_eq!(rows.needs[0].key, InboxKey::Task(awaiting_id));
        assert_eq!(rows.needs[0].task_name, "triage");
        assert_eq!(rows.needs[0].agent.as_deref(), Some("opencode"));
        assert_eq!(rows.needs[0].attempt, Some(1));
        assert_eq!(rows.needs[0].since, at(-120));
        assert_eq!(rows.needs[0].detail.as_deref(), Some("Approve `git push`?"));
        assert!(rows.needs[0].detail_is_permission);
        assert_eq!(rows.needs[1].status, StatusKind::Failed);
        assert_eq!(rows.needs[1].detail.as_deref(), Some("exit code 1"));
        assert!(!rows.needs[1].detail_is_permission);
    }

    #[test]
    fn failures_older_than_the_window_are_only_recent() {
        let now = at(0);
        let mut old = task(1, "nightly", TaskStatus::Failed, at(-2_000));
        old.finished_at = Some(at(-(FAILED_WINDOW_HOURS * 60 + 1)));
        let fresh = task(2, "fresh", TaskStatus::Succeeded, at(-10));
        let fresh_id = fresh.id;
        let old_id = old.id;

        let rows = build(&[old, fresh], &[], None, now);
        assert!(rows.needs.is_empty());
        assert_eq!(rows.recent.len(), 2);
        assert_eq!(rows.recent[0].task_id, fresh_id);
        assert_eq!(rows.recent[1].task_id, old_id);
    }

    /// A launch failure is a broken workflow, not a transient run failure: it
    /// stays in Needs you past the 24h window with the full target and parent.
    #[test]
    fn launch_failures_stay_in_needs_you_past_the_window() {
        let now = at(0);
        let mut parent = task(
            1,
            "repo:favetto/triage_issues",
            TaskStatus::Succeeded,
            at(-4_000),
        );
        parent.finished_at = Some(at(-3_999));
        let mut child = task(2, "repo:Favetto/plan_issue", TaskStatus::Failed, at(-3_998));
        child.parent_id = Some(parent.id);
        child.root_id = Some(parent.id);
        child.finished_at = Some(at(-(FAILED_WINDOW_HOURS * 60 + 5)));
        child.error = Some("task 'repo:Favetto/plan_issue' not found in the catalog".to_string());
        child.failure = Some(Failure::new(
            FailureKind::InvalidInput,
            "task 'repo:Favetto/plan_issue' not found in the catalog",
        ));

        let rows = build(&[parent, child], &[], None, now);
        assert!(rows
            .needs
            .iter()
            .any(|row| row.key.task_id() == Some(Uuid::from_u128(2))));
        let row = rows
            .needs
            .iter()
            .find(|row| row.key.task_id() == Some(Uuid::from_u128(2)))
            .unwrap();
        assert_eq!(row.task_name, "repo:Favetto/plan_issue");
        let detail = row.detail.as_deref().unwrap();
        assert!(detail.contains("repo:Favetto/plan_issue"));
        assert!(detail.contains("not found in the catalog"));
        assert!(detail.contains("which succeeded"));
    }

    #[test]
    fn recent_is_newest_first_capped_and_excludes_needs_you() {
        let now = at(0);
        let mut awaiting = task(999, "blocked", TaskStatus::AwaitingInput, at(-5));
        awaiting.started_at = Some(at(-5));
        let mut tasks = vec![awaiting];
        for index in 0..(RECENT_LIMIT + 5) {
            tasks.push(task(
                100 + index as u128,
                "run",
                TaskStatus::Succeeded,
                at(-(index as i64) - 1),
            ));
        }

        let rows = build(&tasks, &[], None, now);
        assert_eq!(rows.needs.len(), 1);
        assert_eq!(rows.recent.len(), RECENT_LIMIT);
        assert!(rows
            .recent
            .iter()
            .all(|row| row.task_id != Uuid::from_u128(999)));
        assert!(rows
            .recent
            .windows(2)
            .all(|pair| pair[0].created_at >= pair[1].created_at));
    }

    #[test]
    fn an_unknown_status_is_neutral_and_never_color_only() {
        let status = StatusKind::from_label(Some("quarantined"));
        assert_eq!(status, StatusKind::Neutral);
        assert_eq!(status.label(), "unknown");
        assert!(!status.icon().is_empty());
        assert_eq!(StatusKind::from_label(None), StatusKind::Neutral);
        assert_eq!(
            StatusKind::from_task_status(TaskStatus::AwaitingInput),
            StatusKind::AwaitingInput
        );
    }

    #[test]
    fn a_blocked_session_without_a_task_row_is_shown_neutrally() {
        let now = at(0);
        let sessions = vec![session(
            "s-orphan",
            None,
            "claude",
            Some(permission("Allow?")),
        )];
        let rows = build(&[], &sessions, None, now);
        assert_eq!(rows.needs.len(), 1);
        assert_eq!(rows.needs[0].task_name, "Unknown task");
        assert_eq!(rows.needs[0].key, InboxKey::Session("s-orphan".to_string()));
        assert!(rows.needs[0].key.task_id().is_none());
        assert_eq!(rows.needs[0].agent.as_deref(), Some("claude"));
    }

    #[test]
    fn a_blocked_session_wins_even_when_the_task_still_says_running() {
        let now = at(0);
        let mut running = task(5, "repo:favetto/live", TaskStatus::Running, at(-20));
        running.started_at = Some(at(-20));
        let running_id = running.id;
        let sessions = vec![session(
            "s5",
            Some(running_id),
            "opencode",
            Some(permission("Allow the deploy?")),
        )];

        let rows = build(&[running], &sessions, None, now);
        assert_eq!(rows.needs.len(), 1);
        assert_eq!(rows.needs[0].key, InboxKey::Task(running_id));
        assert_eq!(rows.needs[0].status, StatusKind::AwaitingInput);
        assert_eq!(rows.needs[0].task_name, "live");
        assert_eq!(rows.needs[0].detail.as_deref(), Some("Allow the deploy?"));
        assert!(rows.needs[0].detail_is_permission);
        assert!(rows.recent.is_empty());
    }

    #[test]
    fn a_stale_awaiting_session_does_not_override_a_finished_task() {
        let now = at(0);
        let done = task(1, "done", TaskStatus::Succeeded, at(-10));
        let sessions = vec![session(
            "s1",
            Some(done.id),
            "opencode",
            Some(permission("already answered")),
        )];
        let rows = build(&[done], &sessions, None, now);
        assert!(rows.needs.is_empty());
        assert_eq!(rows.recent.len(), 1);
    }

    #[test]
    fn recent_usage_cost_comes_from_the_matching_session() {
        let now = at(0);
        let mut done = task(7, "repo:favetto/costly", TaskStatus::Succeeded, at(-10));
        done.started_at = Some(at(-9));
        done.finished_at = Some(at(-2));
        let mut live = session("s7", Some(done.id), "opencode", None);
        live.usage = Some(AgentUsage {
            cost_usd: Some(0.0123),
            ..AgentUsage::default()
        });

        let rows = build(&[done], &[live], None, now);
        let row = rows.recent.first().expect("recent row");
        assert_eq!(row.repo.as_deref(), Some("favetto"));
        assert_eq!(row.name, "costly");
        assert_eq!(row.duration, Some(Duration::from_secs(7 * 60)));
        assert_eq!(format_cost(row.cost.as_ref()).as_deref(), Some("$0.0123"));
    }

    #[test]
    fn running_tasks_use_the_current_time_for_duration() {
        let now = at(0);
        let mut running = task(1, "live", TaskStatus::Running, at(-30));
        running.started_at = Some(at(-30));
        let rows = build(&[running], &[], None, now);
        assert_eq!(rows.recent[0].duration, Some(Duration::from_secs(30 * 60)));
    }

    #[test]
    fn selectable_keys_cover_both_groups_in_visual_order() {
        let now = at(0);
        let mut awaiting = task(1, "blocked", TaskStatus::AwaitingInput, at(-10));
        awaiting.started_at = Some(at(-10));
        let done = task(2, "done", TaskStatus::Succeeded, at(-5));
        let done_id = done.id;
        let rows = build(&[awaiting, done], &[], None, now);
        assert_eq!(
            rows.keys(),
            vec![InboxKey::Task(Uuid::from_u128(1)), InboxKey::Task(done_id)]
        );
    }

    #[test]
    fn the_active_repo_scopes_needs_you_and_recent_rows() {
        let now = at(0);
        let mut local = task(1, "repo:favetto/triage", TaskStatus::AwaitingInput, at(-10));
        local.started_at = Some(at(-10));
        let failed = task(2, "repo:web/release", TaskStatus::Failed, at(-5));
        let global = task(3, "nightly", TaskStatus::Succeeded, at(-2));
        let orphan = session("s-orphan", None, "claude", Some(permission("Allow?")));

        let scoped = build(&[local, failed, global], &[orphan], Some("favetto"), now);
        assert_eq!(scoped.needs.len(), 1);
        assert_eq!(scoped.needs[0].task_name, "triage");
        assert_eq!(scoped.recent.len(), 0);

        let all = build(
            &[
                task(4, "repo:favetto/triage", TaskStatus::Succeeded, at(-10)),
                task(5, "repo:web/release", TaskStatus::Succeeded, at(-5)),
                task(6, "nightly", TaskStatus::Succeeded, at(-2)),
            ],
            &[session(
                "s-orphan",
                None,
                "claude",
                Some(permission("Allow?")),
            )],
            None,
            now,
        );
        assert_eq!(all.needs.len(), 1, "the orphan session is visible globally");
        assert_eq!(
            all.recent.len(),
            3,
            "global rows are visible with no active repo"
        );
    }

    #[test]
    fn repo_names_split_only_well_formed_prefixes() {
        assert_eq!(task_repo("repo:favetto/triage"), Some("favetto"));
        assert_eq!(task_repo("repo:web/pipelines/plan"), Some("web"));
        assert_eq!(task_repo("triage"), None);
        assert_eq!(task_repo("repo:noslash"), None);
        assert_eq!(task_display_name("repo:favetto/triage"), "triage");
        assert_eq!(
            task_display_name("repo:web/pipelines/plan"),
            "pipelines/plan"
        );
        assert_eq!(task_display_name("repo:"), "repo:");
        assert_eq!(task_display_name("triage"), "triage");
    }

    #[test]
    fn formatting_is_compact_and_never_panics_on_unknowns() {
        assert_eq!(format_elapsed(-1), "0s");
        assert_eq!(format_elapsed(12), "12s");
        assert_eq!(format_elapsed(3 * 60), "3m");
        assert_eq!(format_elapsed(2 * 3_600 + 5 * 60), "2h 05m");
        assert_eq!(format_elapsed(3 * 86_400 + 4 * 3_600), "3d 4h");
        assert_eq!(format_duration(None), "—");
        assert_eq!(format_cost(None), None);
        assert_eq!(format_cost(Some(&AgentCost::usd(f64::NAN))), None);
        assert_eq!(
            format_cost(Some(&AgentCost::usd(0.0))).as_deref(),
            Some("$0.0000")
        );
        assert_eq!(
            format_cost(Some(&AgentCost::usd(0.5))).as_deref(),
            Some("$0.5000")
        );
        assert_eq!(
            format_cost(Some(&AgentCost::new(2.5, "credits"))).as_deref(),
            Some("2.5000 credits")
        );
    }
}
