//! The Tasks model: every run, one status filter and a fuzzy search.
//!
//! [`ClientState`](crate::state::ClientState) stores the raw `tasks.list` rows
//! and the flattened `agents.list` sessions; this module turns them into the
//! rows the Tasks surface renders. It is deliberately free of GPUI types so the
//! filter counts, the fuzzy match and the selection math are unit-tested without
//! a window (docs/design/gpui-client.md §4, "Tests").
//!
//! One dimension, no filter maze (docs §2, Hick's Law): five segments over the
//! daemon's typed [`TaskStatus`]. Counts are computed over the *search-matched*
//! rows, so a segment's badge is always the number of rows selecting it shows.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use favetto_wire::model::{AgentSessionInfo, Task, TaskStatus};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use uuid::Uuid;

use crate::inbox::{self, StatusKind};

/// A task command that needs explicit confirmation before the RPC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskAction {
    /// `tasks.cancel` on a live (non-terminal) task.
    Cancel,
    /// `tasks.retry` on a terminal task.
    Retry,
}

/// The status of a row, as the list renders it. The typed [`TaskStatus`] cannot
/// carry an unknown value, so this never hides a row or panics (docs §2,
/// Postel's Law for the agent string, which is arbitrary text).
pub fn status_kind(status: TaskStatus) -> StatusKind {
    StatusKind::from_task_status(status)
}

/// Whether a status is finished, so `tasks.retry` (not `tasks.cancel`) applies.
pub fn is_terminal(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled
    )
}

/// `tasks.cancel` is accepted while the run can still make progress.
pub fn can_cancel(status: TaskStatus) -> bool {
    !is_terminal(status)
}

/// `tasks.retry` is accepted for a terminal run, matching the daemon's check.
pub fn can_retry(status: TaskStatus) -> bool {
    is_terminal(status)
}

/// The single segmented control on the Tasks surface.
///
/// Pending and cancelled rows deliberately have no segment of their own: the
/// daemon's six states would turn one dimension into a filter maze (docs §2,
/// Hick's Law). They stay visible under **All** and keep their row label.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TaskFilter {
    /// Every run.
    #[default]
    All,
    /// `TaskStatus::Running`.
    Running,
    /// `TaskStatus::AwaitingInput`.
    Awaiting,
    /// `TaskStatus::Failed`.
    Failed,
    /// `TaskStatus::Succeeded`.
    Succeeded,
}

impl TaskFilter {
    /// The segments in display order.
    pub const ALL: [TaskFilter; 5] = [
        TaskFilter::All,
        TaskFilter::Running,
        TaskFilter::Awaiting,
        TaskFilter::Failed,
        TaskFilter::Succeeded,
    ];

    /// The number of segments (also the length of [`TaskList::counts`]).
    pub const COUNT: usize = Self::ALL.len();

    /// The segment label.
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Running => "Running",
            Self::Awaiting => "Awaiting input",
            Self::Failed => "Failed",
            Self::Succeeded => "Succeeded",
        }
    }

    /// Whether a task with `status` belongs to this segment.
    pub fn matches(self, status: TaskStatus) -> bool {
        match self {
            Self::All => true,
            Self::Running => status == TaskStatus::Running,
            Self::Awaiting => status == TaskStatus::AwaitingInput,
            Self::Failed => status == TaskStatus::Failed,
            Self::Succeeded => status == TaskStatus::Succeeded,
        }
    }

    /// Position in [`TaskFilter::ALL`] (the counts array).
    pub fn index(self) -> usize {
        match self {
            Self::All => 0,
            Self::Running => 1,
            Self::Awaiting => 2,
            Self::Failed => 3,
            Self::Succeeded => 4,
        }
    }

    /// Step through the segments, wrapping at both ends.
    pub fn cycle(self, delta: isize) -> Self {
        let index = (self.index() as isize + delta).rem_euclid(Self::COUNT as isize);
        Self::ALL[index as usize]
    }
}

/// One row of the Tasks list: the task plus the live-session metadata the wire
/// only exposes through `agents.list`.
#[derive(Clone, Debug, PartialEq)]
pub struct TaskRow {
    /// The task id (the stable selection key).
    pub id: Uuid,
    /// The display status (icon + label always accompany the color).
    pub status: TaskStatus,
    /// The task name with only a well-formed repo prefix stripped.
    pub name: String,
    /// The full wire name (`repo:favetto/triage`), used by the fuzzy search.
    pub full_name: String,
    /// The repo namespace of a repo-local task.
    pub repo: Option<String>,
    /// The agent that owns a session for this task, when there is one.
    pub agent: Option<String>,
    /// How many execution attempts the task has had.
    pub attempt: u32,
    /// Live elapsed for a running task, total for a finished one.
    pub duration: Option<Duration>,
    /// Usage cost summed from the task's live sessions, when reported.
    pub cost_usd: Option<f64>,
    /// Creation time, the tie-break for the newest-first order.
    pub created_at: DateTime<Utc>,
}

impl TaskRow {
    /// The display status kind, for the icon + label + semantic color.
    pub fn status_kind(&self) -> StatusKind {
        status_kind(self.status)
    }
}

/// The query-filtered, status-filtered rows plus the counts for every segment.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TaskList {
    /// Rows for the active filter (query-relevant first, then newest first).
    pub rows: Vec<TaskRow>,
    /// Per-segment counts over the query-matched set, indexed like
    /// [`TaskFilter::ALL`]. Selecting a segment shows exactly this many rows.
    pub counts: [usize; TaskFilter::COUNT],
    /// Every task in the snapshot, ignoring the query and the filter.
    pub total: usize,
    /// Tasks matching the query, ignoring the status filter.
    pub matched: usize,
}

impl TaskList {
    /// Nothing has run: the calm empty state, not a "no matches" message.
    pub fn is_truly_empty(&self) -> bool {
        self.total == 0
    }

    /// There are runs, but none survive the query and the active segment.
    pub fn is_filtered_empty(&self) -> bool {
        self.total > 0 && self.rows.is_empty()
    }

    /// The position of a task id in the current rows.
    pub fn position(&self, id: Uuid) -> Option<usize> {
        self.rows.iter().position(|row| row.id == id)
    }

    /// Resolve the highlighted row: keep the id when it is still visible (so a
    /// live update never moves the highlight), otherwise fall back to the last
    /// known index, clamped to the new list.
    pub fn resolve_index(&self, selected: Option<Uuid>, fallback: usize) -> Option<usize> {
        if self.rows.is_empty() {
            return None;
        }
        if let Some(index) = selected.and_then(|id| self.position(id)) {
            return Some(index);
        }
        Some(fallback.min(self.rows.len() - 1))
    }

    /// The highlighted task id, resolved through [`TaskList::resolve_index`].
    pub fn selected_id(&self, selected: Option<Uuid>, fallback: usize) -> Option<Uuid> {
        self.resolve_index(selected, fallback)
            .map(|index| self.rows[index].id)
    }
}

/// Build the Tasks rows from a snapshot.
///
/// `query` matches fuzzily over the task name (full and display form), the
/// agent and the repo; every whitespace-separated term must match at least one
/// field. An empty query keeps the daemon's newest-first order. The filter is
/// applied after matching, so the counts stay search-scoped and always agree
/// with the rows a segment shows.
pub fn build(
    tasks: &[Task],
    sessions: &[AgentSessionInfo],
    query: &str,
    filter: TaskFilter,
    now: DateTime<Utc>,
) -> TaskList {
    let mut agents: HashMap<&str, &str> = HashMap::new();
    let mut costs: HashMap<&str, f64> = HashMap::new();
    for session in sessions {
        let Some(task_id) = session.task_id.as_deref() else {
            continue;
        };
        agents.entry(task_id).or_insert(session.agent.as_str());
        if let Some(cost) = session
            .usage
            .as_ref()
            .and_then(|usage| usage.cost_usd)
            .filter(|cost| cost.is_finite())
        {
            *costs.entry(task_id).or_insert(0.0) += cost;
        }
    }

    let terms: Vec<&str> = query.split_whitespace().collect();
    let matcher = SkimMatcherV2::default();

    let mut counts = [0usize; TaskFilter::COUNT];
    let mut matched = 0usize;
    let mut scored: Vec<(i64, TaskRow)> = Vec::new();

    for task in tasks {
        let key = task.id.to_string();
        let repo = inbox::task_repo(&task.name).map(str::to_string);
        let name = inbox::task_display_name(&task.name).to_string();
        let agent = agents.get(key.as_str()).map(|agent| (*agent).to_string());

        let score = match score_row(&matcher, &terms, &task.name, &name, &repo, &agent) {
            Some(score) => score,
            None => continue,
        };

        matched += 1;
        counts[TaskFilter::All.index()] += 1;
        for filter in TaskFilter::ALL {
            if filter != TaskFilter::All && filter.matches(task.status) {
                counts[filter.index()] += 1;
            }
        }

        scored.push((
            score,
            TaskRow {
                id: task.id,
                status: task.status,
                name,
                full_name: task.name.clone(),
                repo,
                agent,
                attempt: task.attempt,
                duration: inbox::task_duration(task, now),
                cost_usd: costs
                    .get(key.as_str())
                    .copied()
                    .filter(|cost| cost.is_finite()),
                created_at: task.created_at,
            },
        ));
    }

    let query_is_empty = terms.is_empty();
    scored.sort_by(|(left_score, left), (right_score, right)| {
        // With no query, newest first; with one, relevance first and recency
        // as the tie-break.
        let ordering = if query_is_empty {
            right.created_at.cmp(&left.created_at)
        } else {
            right_score
                .cmp(left_score)
                .then_with(|| right.created_at.cmp(&left.created_at))
        };
        ordering.then_with(|| right.id.cmp(&left.id))
    });

    let rows = scored
        .into_iter()
        .map(|(_, row)| row)
        .filter(|row| filter.matches(row.status))
        .collect();

    TaskList {
        rows,
        counts,
        total: tasks.len(),
        matched,
    }
}

/// Score one task against every whitespace term. Every term must hit at least
/// one haystack (name, display name, repo or agent); the scores are summed so
/// a task matching several terms sorts first.
fn score_row(
    matcher: &SkimMatcherV2,
    terms: &[&str],
    full_name: &str,
    name: &str,
    repo: &Option<String>,
    agent: &Option<String>,
) -> Option<i64> {
    if terms.is_empty() {
        return Some(0);
    }
    let haystacks = [
        Some(full_name),
        Some(name),
        repo.as_deref(),
        agent.as_deref(),
    ];
    let mut total = 0i64;
    for term in terms {
        let best = haystacks
            .iter()
            .flatten()
            .filter_map(|haystack| matcher.fuzzy_match(haystack, term))
            .max()?;
        total = total.saturating_add(best);
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use favetto_wire::model::{AgentSessionInfo, AgentUsage};

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

    fn session(id: &str, task_id: Option<Uuid>, agent: &str) -> AgentSessionInfo {
        AgentSessionInfo {
            id: id.to_string(),
            agent: agent.to_string(),
            backend: Default::default(),
            task_id: task_id.map(|id| id.to_string()),
            running: true,
            headless: true,
            session_id: None,
            awaiting_input: None,
            activity: None,
            usage: None,
            load_state: Default::default(),
            load_error: None,
        }
    }

    fn fixture() -> Vec<Task> {
        vec![
            task(1, "repo:favetto/triage", TaskStatus::Succeeded, at(-60)),
            task(2, "repo:favetto/build", TaskStatus::Succeeded, at(-50)),
            task(3, "repo:favetto/deploy", TaskStatus::Running, at(-40)),
            task(4, "repo:web/check", TaskStatus::AwaitingInput, at(-30)),
            task(5, "repo:web/release", TaskStatus::Failed, at(-20)),
            task(6, "nightly", TaskStatus::Cancelled, at(-10)),
        ]
    }

    #[test]
    fn filter_counts_match_the_rows_each_segment_shows() {
        let tasks = fixture();
        let now = at(0);
        let all = build(&tasks, &[], "", TaskFilter::All, now);
        assert_eq!(all.total, 6);
        assert_eq!(all.matched, 6);
        assert_eq!(all.counts, [6, 1, 1, 1, 2]);
        assert_eq!(all.rows.len(), 6);

        for filter in TaskFilter::ALL {
            let list = build(&tasks, &[], "", filter, now);
            let expected = list.counts[filter.index()];
            assert_eq!(
                list.rows.len(),
                expected,
                "{} has a stale count",
                filter.label()
            );
            assert!(list.rows.iter().all(|row| filter.matches(row.status)));
        }
    }

    #[test]
    fn search_matches_name_agent_repo_and_multiple_terms() {
        let tasks = fixture();
        let sessions = vec![
            session("s1", Some(Uuid::from_u128(1)), "opencode"),
            session("s2", Some(Uuid::from_u128(4)), "claude"),
        ];
        let now = at(0);
        let query = |query: &str| build(&tasks, &sessions, query, TaskFilter::All, now).rows;

        let triage = query("triage");
        assert_eq!(triage.len(), 1);
        assert_eq!(triage[0].id, Uuid::from_u128(1));

        let by_agent = query("claude");
        assert_eq!(by_agent.len(), 1);
        assert_eq!(by_agent[0].id, Uuid::from_u128(4));
        assert_eq!(by_agent[0].agent.as_deref(), Some("claude"));

        let by_repo = query("web");
        assert_eq!(by_repo.len(), 2);
        assert!(by_repo.iter().all(|row| row.repo.as_deref() == Some("web")));

        let multi = query("web release");
        assert_eq!(multi.len(), 1);
        assert_eq!(multi[0].id, Uuid::from_u128(5));

        assert!(query("zzz-not-a-run").is_empty());
    }

    #[test]
    fn searching_preserves_the_filter_and_scopes_the_counts() {
        let tasks = vec![
            task(1, "repo:favetto/alpha", TaskStatus::Failed, at(-10)),
            task(2, "repo:favetto/alpha-two", TaskStatus::Succeeded, at(-5)),
        ];
        let list = build(&tasks, &[], "alpha", TaskFilter::Failed, at(0));
        assert_eq!(list.matched, 2);
        assert_eq!(list.counts, [2, 0, 0, 1, 1]);
        assert_eq!(list.rows.len(), 1);
        assert_eq!(list.rows[0].id, Uuid::from_u128(1));
        assert_eq!(list.rows[0].name, "alpha");

        // The All segment keeps both, so the filter is genuinely preserved.
        let all = build(&tasks, &[], "alpha", TaskFilter::All, at(0));
        assert_eq!(all.rows.len(), 2);
    }

    #[test]
    fn filtered_empty_is_distinct_from_truly_empty() {
        let empty = build(&[], &[], "", TaskFilter::All, at(0));
        assert!(empty.is_truly_empty());
        assert!(!empty.is_filtered_empty());

        let tasks = fixture();
        let unmatched = build(&tasks, &[], "zzz-not-a-run", TaskFilter::All, at(0));
        assert!(!unmatched.is_truly_empty());
        assert!(unmatched.is_filtered_empty());
        assert_eq!(unmatched.matched, 0);
        assert_eq!(unmatched.counts, [0; TaskFilter::COUNT]);
    }

    #[test]
    fn rows_are_newest_first_without_a_query() {
        let list = build(&fixture(), &[], "", TaskFilter::All, at(0));
        let ids: Vec<_> = list.rows.iter().map(|row| row.id).collect();
        assert_eq!(ids.first(), Some(&Uuid::from_u128(6)));
        assert_eq!(ids.last(), Some(&Uuid::from_u128(1)));
        assert!(list
            .rows
            .windows(2)
            .all(|pair| pair[0].created_at >= pair[1].created_at));
    }

    #[test]
    fn selection_survives_updates_until_the_id_disappears() {
        let now = at(0);
        let tasks = vec![
            task(1, "one", TaskStatus::Succeeded, at(-30)),
            task(2, "two", TaskStatus::Succeeded, at(-20)),
            task(3, "three", TaskStatus::Succeeded, at(-10)),
        ];
        let list = build(&tasks, &[], "", TaskFilter::All, now);
        let two = Uuid::from_u128(2);
        assert_eq!(list.selected_id(Some(two), 0), Some(two));
        assert_eq!(list.resolve_index(Some(two), 0), Some(1));

        let without_two: Vec<_> = tasks.into_iter().filter(|task| task.id != two).collect();
        let list = build(&without_two, &[], "", TaskFilter::All, now);
        // The id is gone: keep the index, clamped to the new list.
        assert_eq!(list.resolve_index(Some(two), 1), Some(1));
        assert_eq!(list.resolve_index(Some(two), 999), Some(1));
        assert_eq!(list.resolve_index(None, 999), Some(1));

        let empty = build(&[], &[], "", TaskFilter::All, now);
        assert_eq!(empty.selected_id(Some(two), 1), None);
    }

    #[test]
    fn terminal_rows_are_retryable_and_live_rows_cancellable() {
        for status in [
            TaskStatus::Pending,
            TaskStatus::Running,
            TaskStatus::AwaitingInput,
        ] {
            assert!(can_cancel(status));
            assert!(!can_retry(status));
            assert!(!is_terminal(status));
        }
        for status in [
            TaskStatus::Succeeded,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            assert!(!can_cancel(status));
            assert!(can_retry(status));
            assert!(is_terminal(status));
        }
    }

    #[test]
    fn rows_never_panic_on_unusual_agent_strings_and_are_sorted() {
        let tasks = vec![
            task(1, "first", TaskStatus::Running, at(-20)),
            task(2, "second", TaskStatus::Running, at(-10)),
        ];
        let sessions = vec![
            session("s1", Some(Uuid::from_u128(1)), ""),
            session("s2", Some(Uuid::from_u128(2)), "agent/with spaces ⚙"),
        ];
        let list = build(&tasks, &sessions, "agent", TaskFilter::All, at(0));
        assert_eq!(list.rows.len(), 1);
        assert_eq!(list.rows[0].agent.as_deref(), Some("agent/with spaces ⚙"));
        assert_eq!(list.rows[0].status_kind(), StatusKind::Running);
    }

    #[test]
    fn usage_cost_is_summed_from_the_matching_sessions() {
        let now = at(0);
        let mut running = task(7, "repo:favetto/costly", TaskStatus::Running, at(-10));
        running.started_at = Some(at(-10));
        let mut first = session("s7", Some(running.id), "opencode");
        first.usage = Some(AgentUsage {
            cost_usd: Some(0.0123),
            ..AgentUsage::default()
        });
        let mut second = session("s8", Some(running.id), "opencode");
        second.usage = Some(AgentUsage {
            cost_usd: Some(0.0077),
            ..AgentUsage::default()
        });

        let list = build(&[running], &[first, second], "", TaskFilter::All, now);
        let row = list.rows.first().expect("row");
        assert_eq!(row.cost_usd, Some(0.02));
        assert_eq!(inbox::format_cost(row.cost_usd).as_deref(), Some("$0.02"));
        assert_eq!(row.duration, Some(Duration::from_secs(10 * 60)));
    }

    #[test]
    fn filter_cycling_wraps() {
        assert_eq!(TaskFilter::All.cycle(-1), TaskFilter::Succeeded);
        assert_eq!(TaskFilter::Succeeded.cycle(1), TaskFilter::All);
        assert_eq!(TaskFilter::All.cycle(2), TaskFilter::Awaiting);
        assert_eq!(TaskFilter::All.label(), "All");
        assert_eq!(TaskFilter::Awaiting.label(), "Awaiting input");
    }
}
