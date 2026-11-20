//! The Workflows model: the roots seen in the current task snapshot, one
//! root's runtime inspect rows, their five buckets, and the catalog DAG source.
//!
//! The daemon already does the hard part: `workflow.inspect` splits the root's
//! instances into `ready`, `running`, `failed` and `blocked`. This module only
//! joins that payload with the cached `tasks.list` rows (which carry
//! `parent_id` for dependency labels), groups the terminal leftovers, and keeps
//! the optimistic cancel/retry state honest. It is deliberately free of GPUI
//! types so bucket counts, progress and selection are unit-tested without a
//! window (docs/design/gpui-client.md §4, "Tests").
//!
//! Progress is never faked: `done / total` is the number of succeeded
//! instances over the instances the daemon returned. A failed or cancelled row
//! is terminal but not "done" (docs §2, Goal-Gradient).

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use favetto_wire::model::{Task, TaskStatus};
use favetto_wire::workflow::{WorkflowInspect, WorkflowState};
use uuid::Uuid;

use crate::inbox::{self, StatusKind};
use crate::tasks;

/// One of the five sections of a root's runtime inspect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WorkflowBucket {
    /// Pending with no active `needs` predecessor: the daemon will claim it.
    Ready,
    /// Running or awaiting input.
    Running,
    /// Failed and retryable.
    Failed,
    /// Pending behind an active `needs` predecessor.
    Blocked,
    /// Succeeded or cancelled: the instance will not move without a retry.
    Terminal,
}

impl WorkflowBucket {
    /// The sections in display order. Attention first, history last: the five
    /// chunks mirror the daemon's own buckets (docs §2, Chunking).
    pub const ALL: [WorkflowBucket; 5] = [
        WorkflowBucket::Ready,
        WorkflowBucket::Running,
        WorkflowBucket::Failed,
        WorkflowBucket::Blocked,
        WorkflowBucket::Terminal,
    ];

    /// The section heading.
    pub fn label(self) -> &'static str {
        match self {
            Self::Ready => "Ready",
            Self::Running => "Running",
            Self::Failed => "Failed",
            Self::Blocked => "Blocked",
            Self::Terminal => "Terminal",
        }
    }

    /// What the section means when it has no rows, so an empty bucket is still
    /// an explanation rather than a blank gap.
    pub fn empty(self) -> &'static str {
        match self {
            Self::Ready => "Nothing queued and waiting for a slot.",
            Self::Running => "No instance is executing or blocked on a prompt.",
            Self::Failed => "No failed instances.",
            Self::Blocked => "No instance is waiting on another instance.",
            Self::Terminal => "No instance has finished yet.",
        }
    }

    /// Derive a bucket from a status. `blocked` only matters for pending rows;
    /// every other state is already unambiguous.
    pub fn from_status(status: TaskStatus, blocked: bool) -> Self {
        match status {
            TaskStatus::Pending if blocked => Self::Blocked,
            TaskStatus::Pending => Self::Ready,
            TaskStatus::Running | TaskStatus::AwaitingInput => Self::Running,
            TaskStatus::Failed => Self::Failed,
            TaskStatus::Succeeded | TaskStatus::Cancelled => Self::Terminal,
        }
    }
}

/// One instance row of the selected root.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowRow {
    /// The task id (the stable selection key across refreshes).
    pub id: Uuid,
    /// The task name with only a well-formed repo prefix stripped.
    pub name: String,
    /// The live status.
    pub status: TaskStatus,
    /// How many execution attempts the task has had.
    pub attempt: u32,
    /// Bounded outcome text (`Task::error`) when the daemon reports one.
    pub summary: Option<String>,
    /// Display names of the predecessors this instance waited for.
    pub dependencies: Vec<String>,
    /// The section this row renders under.
    pub bucket: WorkflowBucket,
    /// Whether the daemon considered the pending row blocked by a predecessor.
    /// Retained so a live `pending` push cannot accidentally unblock it.
    pub blocked: bool,
    /// True while a retry RPC for this row is in flight (the optimistic marker).
    pub retry_pending: bool,
}

impl WorkflowRow {
    /// The status icon + label vocabulary shared with Inbox and Tasks.
    pub fn status_kind(&self) -> StatusKind {
        StatusKind::from_task_status(self.status)
    }
}

/// One rendered section: a bucket and the rows in it, in inspect order.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowSection {
    /// The bucket.
    pub bucket: WorkflowBucket,
    /// Rows in the order the daemon returned them.
    pub rows: Vec<WorkflowRow>,
}

/// One root in the Workflows list, aggregated from the current task snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowRoot {
    /// The workflow root id (`root_or_self`).
    pub root_id: Uuid,
    /// The root task's display name.
    pub name: String,
    /// The overall root state, derived like the daemon's `workflow_state`.
    pub state: WorkflowState,
    /// Every instance seen for this root in the snapshot.
    pub total: usize,
    /// Succeeded instances (`done` in `done / total`).
    pub done: usize,
    /// Pending/running/awaiting instances (the cancellable count).
    pub active: usize,
    /// Failed instances.
    pub failed: usize,
    /// The newest instance's creation time, the list's sort key.
    pub newest: DateTime<Utc>,
}

impl WorkflowRoot {
    /// Fraction of the root's instances that succeeded, never guessed.
    pub fn progress(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            self.done as f32 / self.total as f32
        }
    }
}

/// A selected root's runtime inspect joined with cached dependencies and the
/// transient optimistic action state.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowDetail {
    /// The inspected root.
    pub root_id: Uuid,
    /// The root task's display name.
    pub root_name: String,
    /// The overall root state.
    pub state: WorkflowState,
    /// Every instance row.
    pub rows: Vec<WorkflowRow>,
    /// The highlighted instance, preserved by id across refreshes.
    pub selected: Option<Uuid>,
    /// True while the first `workflow.inspect` for this root is in flight.
    pub loading: bool,
    /// The last inspect error; rows are kept when an older view exists.
    pub error: Option<String>,
    /// True while the root-cancel RPC is in flight (every non-terminal row was
    /// marked cancelled optimistically).
    pub pending_cancel: bool,
    /// True when the root was started directly (no `root_id` in the snapshot),
    /// so the surface can explain that it is its own root.
    pub direct_start: bool,
}

impl WorkflowDetail {
    /// A loading placeholder for `root_id`, so switching roots never shows the
    /// previous root's rows under the new name.
    pub fn loading(root_id: Uuid, root_name: String) -> Self {
        Self {
            root_id,
            root_name,
            state: WorkflowState::Running,
            rows: Vec::new(),
            selected: None,
            loading: true,
            error: None,
            pending_cancel: false,
            direct_start: false,
        }
    }

    /// Join one `workflow.inspect` payload with the task snapshot.
    ///
    /// The inspect payload deliberately omits `parent_id` and timestamps; the
    /// snapshot fills the dependency label. `selected` is kept when that
    /// instance is still present, otherwise the first row is highlighted.
    pub fn from_inspect(inspect: &WorkflowInspect, tasks: &[Task], selected: Option<Uuid>) -> Self {
        let blocked: HashSet<Uuid> = inspect.blocked.iter().copied().collect();
        let cached: HashMap<Uuid, &Task> = tasks.iter().map(|task| (task.id, task)).collect();
        let mut names: HashMap<Uuid, String> = inspect
            .tasks
            .iter()
            .map(|task| (task.id, inbox::task_display_name(&task.name).to_string()))
            .collect();
        // A predecessor can live just outside the root's inspect set; the
        // snapshot usually still names it.
        for task in tasks {
            names
                .entry(task.id)
                .or_insert_with(|| inbox::task_display_name(&task.name).to_string());
        }

        let rows: Vec<WorkflowRow> = inspect
            .tasks
            .iter()
            .map(|task| {
                let mut dependencies = Vec::new();
                if let Some(parent) = cached.get(&task.id).and_then(|task| task.parent_id) {
                    if parent != task.id {
                        if let Some(name) = names.get(&parent) {
                            dependencies.push(name.clone());
                        }
                    }
                }
                WorkflowRow {
                    id: task.id,
                    name: names
                        .get(&task.id)
                        .cloned()
                        .unwrap_or_else(|| inbox::task_display_name(&task.name).to_string()),
                    status: task.status,
                    attempt: task.attempt,
                    summary: task.summary.clone(),
                    dependencies,
                    bucket: WorkflowBucket::from_status(task.status, blocked.contains(&task.id)),
                    blocked: blocked.contains(&task.id),
                    retry_pending: false,
                }
            })
            .collect();

        let mut detail = Self {
            root_id: inspect.root_id,
            root_name: inbox::task_display_name(&inspect.root_task).to_string(),
            state: overall_state(&rows),
            selected: None,
            rows,
            loading: false,
            error: None,
            pending_cancel: false,
            direct_start: cached
                .get(&inspect.root_id)
                .is_some_and(|task| task.root_id.is_none()),
        };
        detail.select_preferred(selected);
        detail
    }

    /// Succeeded instances.
    pub fn done(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.status == TaskStatus::Succeeded)
            .count()
    }

    /// Every instance the daemon returned.
    pub fn total(&self) -> usize {
        self.rows.len()
    }

    /// Instances that can no longer progress without a retry.
    pub fn terminal(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| tasks::is_terminal(row.status))
            .count()
    }

    /// Instances `workflow.cancel` would transition (the confirmation count).
    pub fn cancellable(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| tasks::can_cancel(row.status))
            .count()
    }

    /// Fraction of the root's instances that succeeded, never guessed.
    pub fn progress(&self) -> f32 {
        if self.rows.is_empty() {
            0.0
        } else {
            self.done() as f32 / self.rows.len() as f32
        }
    }

    /// The five sections in display order, including empty buckets so counts
    /// always agree with the rows below them.
    pub fn sections(&self) -> Vec<WorkflowSection> {
        WorkflowBucket::ALL
            .iter()
            .map(|bucket| WorkflowSection {
                bucket: *bucket,
                rows: self
                    .rows
                    .iter()
                    .filter(|row| row.bucket == *bucket)
                    .cloned()
                    .collect(),
            })
            .collect()
    }

    /// The rows in visual order (bucket order, then inspect order).
    pub fn ordered_rows(&self) -> Vec<&WorkflowRow> {
        let mut rows = Vec::with_capacity(self.rows.len());
        for bucket in WorkflowBucket::ALL {
            rows.extend(self.rows.iter().filter(|row| row.bucket == bucket));
        }
        rows
    }

    /// The highlighted row, falling back to the first row in visual order.
    pub fn selected_row(&self) -> Option<&WorkflowRow> {
        self.selected
            .and_then(|id| self.rows.iter().find(|row| row.id == id))
            .or_else(|| self.ordered_rows().into_iter().next())
    }

    /// The effective selected id (the stored id or the first visible row).
    pub fn selected_id(&self) -> Option<Uuid> {
        self.selected_row().map(|row| row.id)
    }

    /// Highlight one instance if it is part of this root.
    pub fn select(&mut self, id: Uuid) -> bool {
        if self.rows.iter().any(|row| row.id == id) {
            self.selected = Some(id);
            true
        } else {
            false
        }
    }

    /// Move the highlight by one in visual order, clamped (lists do not wrap).
    pub fn move_selection(&mut self, delta: isize) {
        let ids: Vec<Uuid> = self.ordered_rows().iter().map(|row| row.id).collect();
        if ids.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .and_then(|id| ids.iter().position(|candidate| *candidate == id))
            .unwrap_or(0);
        let next = (current as isize + delta).clamp(0, ids.len() as isize - 1) as usize;
        self.selected = Some(ids[next]);
    }

    /// Select Home (index zero) or End (last index).
    pub fn select_edge(&mut self, last: bool) {
        let ids: Vec<Uuid> = self.ordered_rows().iter().map(|row| row.id).collect();
        self.selected = if last {
            ids.last().copied()
        } else {
            ids.first().copied()
        };
    }

    /// Fold one `task.updated` push into the row immediately (Doherty
    /// Threshold). Returns whether the row changed.
    pub fn apply_task(&mut self, task: &Task) -> bool {
        let Some(row) = self.rows.iter_mut().find(|row| row.id == task.id) else {
            return false;
        };
        let changed = row.status != task.status
            || row.attempt != task.attempt
            || row.summary != task.error
            || row.retry_pending;
        row.status = task.status;
        row.attempt = task.attempt;
        row.summary = task.error.clone();
        if task.status != TaskStatus::Pending {
            row.blocked = false;
        }
        row.bucket = WorkflowBucket::from_status(task.status, row.blocked);
        row.retry_pending = false;
        if changed {
            self.recompute_state();
        }
        changed
    }

    /// Optimistically mark every non-terminal row cancelled. Returns the number
    /// of rows moved.
    pub fn begin_cancel(&mut self) -> usize {
        let mut moved = 0usize;
        for row in &mut self.rows {
            if tasks::can_cancel(row.status) {
                row.status = TaskStatus::Cancelled;
                row.bucket = WorkflowBucket::Terminal;
                row.blocked = false;
                row.retry_pending = false;
                moved += 1;
            }
        }
        if moved > 0 {
            self.pending_cancel = true;
            self.recompute_state();
        }
        moved
    }

    /// Reconcile the root cancel response. The daemon returns only the ids it
    /// actually transitioned; rows it skipped keep whatever a newer push says.
    pub fn finish_cancel(&mut self, cancelled: &[Uuid]) {
        let ids: HashSet<Uuid> = cancelled.iter().copied().collect();
        for row in &mut self.rows {
            if ids.contains(&row.id) {
                row.status = TaskStatus::Cancelled;
                row.bucket = WorkflowBucket::Terminal;
                row.blocked = false;
            }
        }
        self.pending_cancel = false;
        self.recompute_state();
    }

    /// Optimistically move one failed row back to Ready. Returns the prior row
    /// so a failed RPC can restore exactly that state.
    pub fn begin_retry(&mut self, id: Uuid) -> Option<WorkflowRow> {
        let row = self.rows.iter_mut().find(|row| row.id == id)?;
        if row.status != TaskStatus::Failed || row.retry_pending {
            return None;
        }
        let previous = row.clone();
        row.status = TaskStatus::Pending;
        row.bucket = WorkflowBucket::Ready;
        row.blocked = false;
        row.retry_pending = true;
        row.summary = None;
        self.recompute_state();
        Some(previous)
    }

    /// Restore a row after a rejected retry, unless a newer push already moved
    /// it. This mirrors the guarded rollback of the Tasks list.
    pub fn rollback_retry(&mut self, previous: WorkflowRow) {
        if let Some(row) = self.rows.iter_mut().find(|row| row.id == previous.id) {
            if row.status == TaskStatus::Pending && row.retry_pending {
                *row = previous;
                self.recompute_state();
            }
        }
    }

    /// Reconcile a successful retry: the daemon's fresh task carries the real
    /// attempt and status.
    pub fn finish_retry(&mut self, task: &Task) {
        self.apply_task(task);
    }

    #[cfg(test)]
    fn bucket_count(&self, bucket: WorkflowBucket) -> usize {
        self.rows.iter().filter(|row| row.bucket == bucket).count()
    }

    fn select_preferred(&mut self, preferred: Option<Uuid>) {
        self.selected = preferred.filter(|id| self.rows.iter().any(|row| row.id == *id));
        if self.selected.is_none() {
            self.selected = self.ordered_rows().first().map(|row| row.id);
        }
    }

    fn recompute_state(&mut self) {
        self.state = overall_state(&self.rows);
    }
}

/// The daemon's overall-root derivation, kept in sync with `workflow.inspect`.
pub fn overall_state(rows: &[WorkflowRow]) -> WorkflowState {
    if rows.iter().any(|row| {
        matches!(
            row.status,
            TaskStatus::Pending | TaskStatus::Running | TaskStatus::AwaitingInput
        )
    }) {
        WorkflowState::Running
    } else if rows.iter().any(|row| row.status == TaskStatus::Failed) {
        WorkflowState::Failed
    } else if rows.iter().any(|row| row.status == TaskStatus::Cancelled) {
        WorkflowState::Cancelled
    } else {
        WorkflowState::Succeeded
    }
}

/// The root state as the shared status vocabulary (icon + label + color).
pub fn state_kind(state: WorkflowState) -> StatusKind {
    match state {
        WorkflowState::Running => StatusKind::Running,
        WorkflowState::Succeeded => StatusKind::Succeeded,
        WorkflowState::Failed => StatusKind::Failed,
        WorkflowState::Cancelled => StatusKind::Cancelled,
    }
}

/// Group the current task snapshot by `root_or_self`, newest root first.
///
/// There is no `workflow.list` RPC; the roots a user can inspect are exactly
/// the roots whose tasks are in the 500-row snapshot the Inbox and Tasks share.
pub fn roots(tasks: &[Task]) -> Vec<WorkflowRoot> {
    let mut groups: HashMap<Uuid, Vec<&Task>> = HashMap::new();
    for task in tasks {
        groups.entry(task.root_or_self()).or_default().push(task);
    }

    let mut roots: Vec<WorkflowRoot> = groups
        .into_iter()
        .map(|(root_id, tasks)| {
            let root_task = tasks.iter().find(|task| task.id == root_id);
            let name = root_task
                .map(|task| inbox::task_display_name(&task.name))
                .or_else(|| {
                    tasks
                        .first()
                        .map(|task| inbox::task_display_name(&task.name))
                })
                .unwrap_or("workflow")
                .to_string();
            let rows: Vec<WorkflowRow> = tasks
                .iter()
                .map(|task| WorkflowRow {
                    id: task.id,
                    name: inbox::task_display_name(&task.name).to_string(),
                    status: task.status,
                    attempt: task.attempt,
                    summary: task.error.clone(),
                    dependencies: Vec::new(),
                    bucket: WorkflowBucket::from_status(task.status, false),
                    blocked: false,
                    retry_pending: false,
                })
                .collect();
            let newest = tasks
                .iter()
                .map(|task| task.created_at)
                .max()
                .unwrap_or_else(Utc::now);
            WorkflowRoot {
                root_id,
                name,
                state: overall_state(&rows),
                total: rows.len(),
                done: rows
                    .iter()
                    .filter(|row| row.status == TaskStatus::Succeeded)
                    .count(),
                active: rows
                    .iter()
                    .filter(|row| tasks::can_cancel(row.status))
                    .count(),
                failed: rows
                    .iter()
                    .filter(|row| row.status == TaskStatus::Failed)
                    .count(),
                newest,
            }
        })
        .collect();

    roots.sort_by(|left, right| {
        right
            .newest
            .cmp(&left.newest)
            .then_with(|| right.root_id.cmp(&left.root_id))
    });
    roots
}

/// The catalog DAG source (`workflow.get`) and its load state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkflowGraphState {
    /// The Graphviz DOT source.
    pub dot: String,
    /// Where the daemon persists the rendered graph, when it reports one.
    pub path: String,
    /// True while a read is in flight.
    pub loading: bool,
    /// True once a source has been read successfully.
    pub loaded: bool,
    /// The last read error; an older source is kept behind it.
    pub error: Option<String>,
}

impl WorkflowGraphState {
    /// Mark a read in flight without discarding the current source.
    pub fn begin_load(&mut self) {
        self.loading = true;
        self.error = None;
    }

    /// Store a successful `workflow.get`.
    pub fn set(&mut self, dot: String, path: String) {
        self.dot = dot;
        self.path = path;
        self.loading = false;
        self.loaded = true;
        self.error = None;
    }

    /// Record a failed read, keeping any older source visible.
    pub fn fail(&mut self, message: String) {
        self.loading = false;
        self.error = Some(message);
    }

    /// The short legend beside the DOT panel. No Graphviz binary is embedded;
    /// this text keeps the edge vocabulary readable (docs §5 step 08).
    pub const LEGEND: &'static str = "spawn = parent → child · needs = waits for a task · \
needs:succeeded / needs:failed = outcome gate · join = waits for all";

    /// The path line, with a calm fallback when the daemon did not report one.
    pub fn path_label(&self) -> &str {
        if self.path.is_empty() {
            "not persisted yet"
        } else {
            &self.path
        }
    }
}

/// A root cancellation awaiting confirmation. The body names the number of
/// instances the RPC will transition (docs §2: the destructive action names
/// its consequence).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowCancelConfirm {
    /// The root the RPC targets.
    pub root_id: Uuid,
    /// The root task's display name.
    pub root_name: String,
    /// How many non-terminal instances `workflow.cancel` will transition.
    pub cancellable: usize,
}

impl WorkflowCancelConfirm {
    /// The dialog title.
    pub fn title(&self) -> &'static str {
        "Cancel workflow"
    }

    /// The confirm button label.
    pub fn confirm_label(&self) -> &'static str {
        "Cancel root"
    }

    /// Cancel is the only danger-colored control on the surface (Von Restorff).
    pub fn danger(&self) -> bool {
        true
    }

    /// The consequence, with the count and the root name.
    pub fn body(&self) -> String {
        format!(
            "Cancel {} non-terminal instance{} in '{}'? In-flight agents are stopped; \
             instances that already finished keep their outcome.",
            self.cancellable,
            if self.cancellable == 1 { "" } else { "s" },
            self.root_name
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(offset_minutes: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000, 0).unwrap() + chrono::Duration::minutes(offset_minutes)
    }

    fn task(id: u128, name: &str, status: TaskStatus, root: Option<Uuid>) -> Task {
        Task {
            id: Uuid::from_u128(id),
            name: name.to_string(),
            status,
            attempt: 1,
            input: serde_json::Value::Null,
            output: None,
            dedupe_key: None,
            created_at: at(id as i64),
            started_at: None,
            finished_at: None,
            error: None,
            failure: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: root,
            interactive: false,
        }
    }

    fn view(
        id: Uuid,
        name: &str,
        status: TaskStatus,
        attempt: u32,
        summary: Option<&str>,
    ) -> favetto_wire::workflow::WorkflowTaskView {
        favetto_wire::workflow::WorkflowTaskView {
            id,
            name: name.to_string(),
            status,
            attempt,
            summary: summary.map(str::to_string),
        }
    }

    fn two_step() -> (Uuid, Task, Task) {
        let root = Uuid::from_u128(1);
        let root_task = task(1, "repo:demo/build", TaskStatus::Succeeded, None);
        let mut child = task(2, "repo:demo/test", TaskStatus::Pending, Some(root));
        child.parent_id = Some(root);
        (root, root_task, child)
    }

    #[test]
    fn roots_group_by_root_or_self_and_count_progress() {
        let (root, root_task, child) = two_step();
        let roots = roots(&[root_task, child]);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].root_id, root);
        assert_eq!(roots[0].name, "build");
        assert_eq!(roots[0].total, 2);
        assert_eq!(roots[0].done, 1);
        assert_eq!(roots[0].active, 1);
        assert_eq!(roots[0].failed, 0);
        assert_eq!(roots[0].state, WorkflowState::Running);
    }

    #[test]
    fn roots_sort_newest_first_and_include_direct_tasks() {
        let older = task(10, "older", TaskStatus::Succeeded, None);
        let mut newer = task(11, "newer", TaskStatus::Failed, None);
        newer.created_at = at(50);
        let roots = roots(&[older.clone(), newer.clone()]);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].root_id, newer.id);
        assert_eq!(roots[0].state, WorkflowState::Failed);
        assert_eq!(roots[1].root_id, older.id);
        assert_eq!(roots[1].state, WorkflowState::Succeeded);
    }

    #[test]
    fn inspect_places_rows_in_five_buckets_with_dependency_labels() {
        let (root, mut root_task, mut child) = two_step();
        root_task.status = TaskStatus::Failed;
        child.status = TaskStatus::Pending;
        child.parent_id = Some(root);

        let inspect = WorkflowInspect {
            root_id: root,
            root_task: root_task.name.clone(),
            state: WorkflowState::Running,
            tasks: vec![
                view(
                    root_task.id,
                    &root_task.name,
                    TaskStatus::Failed,
                    2,
                    Some("boom"),
                ),
                view(child.id, &child.name, TaskStatus::Pending, 1, None),
            ],
            ready: vec![],
            running: vec![],
            failed: vec![root_task.id],
            blocked: vec![child.id],
        };

        let detail = WorkflowDetail::from_inspect(&inspect, &[root_task.clone(), child], None);
        assert_eq!(detail.root_id, root);
        assert_eq!(detail.root_name, "build");
        assert_eq!(detail.total(), 2);
        assert_eq!(detail.done(), 0);
        assert_eq!(detail.cancellable(), 1);
        assert_eq!(detail.bucket_count(WorkflowBucket::Failed), 1);
        assert_eq!(detail.bucket_count(WorkflowBucket::Blocked), 1);

        let dependencies = &detail
            .rows
            .iter()
            .find(|row| row.id == Uuid::from_u128(2))
            .unwrap()
            .dependencies;
        assert_eq!(dependencies, &vec!["build".to_string()]);
        assert_eq!(detail.selected_id(), Some(root_task.id));
    }

    #[test]
    fn sections_always_render_all_five_buckets_in_order() {
        let (root, root_task, child) = two_step();
        let inspect = WorkflowInspect {
            root_id: root,
            root_task: root_task.name.clone(),
            state: WorkflowState::Running,
            tasks: vec![
                view(
                    root_task.id,
                    &root_task.name,
                    TaskStatus::Succeeded,
                    1,
                    None,
                ),
                view(child.id, &child.name, TaskStatus::Pending, 1, None),
            ],
            ready: vec![child.id],
            running: vec![],
            failed: vec![],
            blocked: vec![],
        };
        let detail = WorkflowDetail::from_inspect(&inspect, &[root_task, child], None);
        let sections = detail.sections();
        assert_eq!(
            sections.iter().map(|s| s.bucket).collect::<Vec<_>>(),
            WorkflowBucket::ALL.to_vec()
        );
        assert_eq!(sections[0].rows.len(), 1);
        assert_eq!(sections[4].rows.len(), 1);
        assert_eq!(detail.done(), 1);
        assert_eq!(detail.progress(), 0.5);
    }

    #[test]
    fn empty_inspect_has_zero_progress_and_no_selection() {
        let root = Uuid::from_u128(3);
        let inspect = WorkflowInspect {
            root_id: root,
            root_task: "empty".to_string(),
            state: WorkflowState::Succeeded,
            tasks: vec![],
            ready: vec![],
            running: vec![],
            failed: vec![],
            blocked: vec![],
        };
        let detail = WorkflowDetail::from_inspect(&inspect, &[], None);
        assert_eq!(detail.progress(), 0.0);
        assert_eq!(detail.selected_id(), None);
        assert_eq!(detail.cancellable(), 0);
    }

    #[test]
    fn cancel_moves_every_non_terminal_row_to_terminal() {
        let (root, mut root_task, mut child) = two_step();
        root_task.status = TaskStatus::Running;
        child.status = TaskStatus::Pending;
        let inspect = WorkflowInspect {
            root_id: root,
            root_task: root_task.name.clone(),
            state: WorkflowState::Running,
            tasks: vec![
                view(root_task.id, &root_task.name, TaskStatus::Running, 1, None),
                view(child.id, &child.name, TaskStatus::Pending, 1, None),
            ],
            ready: vec![child.id],
            running: vec![root_task.id],
            failed: vec![],
            blocked: vec![],
        };
        let mut detail = WorkflowDetail::from_inspect(&inspect, &[root_task, child], None);
        assert_eq!(detail.cancellable(), 2);
        assert_eq!(detail.begin_cancel(), 2);
        assert!(detail.pending_cancel);
        assert_eq!(detail.cancellable(), 0);
        assert_eq!(detail.bucket_count(WorkflowBucket::Terminal), 2);
        assert_eq!(detail.state, WorkflowState::Cancelled);
        detail.finish_cancel(&[Uuid::from_u128(1), Uuid::from_u128(2)]);
        assert!(!detail.pending_cancel);
        assert_eq!(detail.state, WorkflowState::Cancelled);
    }

    #[test]
    fn retry_moves_a_failed_row_to_ready_and_reflects_the_new_attempt() {
        let (root, mut root_task, mut child) = two_step();
        root_task.status = TaskStatus::Failed;
        root_task.attempt = 1;
        child.status = TaskStatus::Pending;

        let inspect = WorkflowInspect {
            root_id: root,
            root_task: root_task.name.clone(),
            state: WorkflowState::Running,
            tasks: vec![
                view(
                    root_task.id,
                    &root_task.name,
                    TaskStatus::Failed,
                    1,
                    Some("boom"),
                ),
                view(child.id, &child.name, TaskStatus::Pending, 1, None),
            ],
            ready: vec![child.id],
            running: vec![],
            failed: vec![root_task.id],
            blocked: vec![],
        };
        let mut detail = WorkflowDetail::from_inspect(&inspect, &[root_task.clone(), child], None);
        detail.begin_retry(root_task.id).expect("retryable row");
        assert_eq!(detail.selected_id(), Some(Uuid::from_u128(2)));
        assert_eq!(detail.bucket_count(WorkflowBucket::Ready), 2);
        assert!(
            detail
                .rows
                .iter()
                .find(|row| row.id == root_task.id)
                .unwrap()
                .retry_pending
        );

        let mut retried = root_task.clone();
        retried.status = TaskStatus::Running;
        retried.attempt = 2;
        retried.error = None;
        detail.finish_retry(&retried);
        let row = detail
            .rows
            .iter()
            .find(|row| row.id == root_task.id)
            .unwrap();
        assert_eq!(row.status, TaskStatus::Running);
        assert_eq!(row.attempt, 2);
        assert!(!row.retry_pending);
        assert_eq!(row.bucket, WorkflowBucket::Running);

        // A rejected retry restores the exact prior row.
        let mut detail =
            WorkflowDetail::from_inspect(&inspect, &[root_task.clone(), retried], None);
        let previous_row = detail.begin_retry(root_task.id).expect("retryable row");
        detail.rollback_retry(previous_row);
        let row = detail
            .rows
            .iter()
            .find(|row| row.id == root_task.id)
            .unwrap();
        assert_eq!(row.status, TaskStatus::Failed);
        assert_eq!(row.bucket, WorkflowBucket::Failed);
    }

    #[test]
    fn apply_task_moves_the_row_and_keeps_the_selection() {
        let (root, root_task, child) = two_step();
        let inspect = WorkflowInspect {
            root_id: root,
            root_task: root_task.name.clone(),
            state: WorkflowState::Running,
            tasks: vec![
                view(
                    root_task.id,
                    &root_task.name,
                    TaskStatus::Succeeded,
                    1,
                    None,
                ),
                view(child.id, &child.name, TaskStatus::Pending, 1, None),
            ],
            ready: vec![child.id],
            running: vec![],
            failed: vec![],
            blocked: vec![],
        };
        let mut detail =
            WorkflowDetail::from_inspect(&inspect, &[root_task, child], Some(Uuid::from_u128(2)));
        assert_eq!(detail.selected_id(), Some(Uuid::from_u128(2)));
        let mut running_child = task(2, "repo:demo/test", TaskStatus::Running, Some(root));
        running_child.attempt = 3;
        assert!(detail.apply_task(&running_child));
        assert_eq!(detail.selected_id(), Some(Uuid::from_u128(2)));
        let row = detail
            .rows
            .iter()
            .find(|row| row.id == running_child.id)
            .unwrap();
        assert_eq!(row.attempt, 3);
        assert_eq!(row.bucket, WorkflowBucket::Running);
    }

    #[test]
    fn state_kinds_match_the_shared_vocabulary() {
        assert_eq!(state_kind(WorkflowState::Running).label(), "running");
        assert_eq!(state_kind(WorkflowState::Failed), StatusKind::Failed);
        assert_eq!(state_kind(WorkflowState::Cancelled).label(), "cancelled");
    }

    #[test]
    fn graph_state_keeps_the_old_source_behind_an_error() {
        let mut graph = WorkflowGraphState::default();
        graph.begin_load();
        assert!(graph.loading);
        graph.set("digraph {}".to_string(), "/tmp/g.dot".to_string());
        assert!(graph.loaded);
        assert_eq!(graph.path_label(), "/tmp/g.dot");

        graph.begin_load();
        graph.fail("offline".to_string());
        assert_eq!(graph.dot, "digraph {}");
        assert!(graph
            .error
            .as_deref()
            .is_some_and(|e| e.contains("offline")));
        assert!(WorkflowGraphState::LEGEND.contains("spawn"));
        assert!(WorkflowGraphState::LEGEND.contains("join"));
    }

    #[test]
    fn cancel_confirmation_counts_and_names_the_root() {
        let confirm = WorkflowCancelConfirm {
            root_id: Uuid::from_u128(1),
            root_name: "build".to_string(),
            cancellable: 2,
        };
        assert_eq!(confirm.title(), "Cancel workflow");
        assert!(confirm.danger());
        assert!(confirm.body().contains("2 non-terminal instances"));
        assert!(confirm.body().contains("'build'"));

        let one = WorkflowCancelConfirm {
            cancellable: 1,
            ..confirm
        };
        assert!(one.body().contains("1 non-terminal instance "));
    }
}
