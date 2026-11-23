//! The Workflows model: the roots seen in the current task snapshot, one
//! root's runtime inspect rows laid out as a left-to-right DAG, and the catalog
//! DAG source.
//!
//! The daemon already does the hard part: `workflow.inspect` returns every
//! instance of a root. This module joins that payload with the cached
//! `tasks.list` rows (which carry `parent_id` and `created_at`), derives the
//! parent → child edges, places the nodes on a deterministic canvas and keeps
//! the optimistic cancel/retry state honest. It is deliberately free of GPUI
//! types so the layout, selection order and blocked markers are unit-tested
//! without a window (docs/design/gpui-client.md §4, "Tests").
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
use crate::launch;
use crate::tasks;

/// The node card width in the graph canvas.
pub const NODE_WIDTH: f32 = 180.0;
/// The node card height in the graph canvas.
pub const NODE_HEIGHT: f32 = 56.0;
/// Horizontal gap between columns.
pub const COLUMN_GAP: f32 = 64.0;
/// Vertical gap between rows of the same column.
pub const ROW_GAP: f32 = 20.0;
/// Padding around the whole graph.
pub const GRAPH_MARGIN: f32 = 24.0;
/// Length of the filled arrowhead at the child end of an edge.
pub const ARROW_LENGTH: f32 = 8.0;
/// Half the base width of the arrowhead, so the visible head is twice as wide.
pub const ARROW_HALF_WIDTH: f32 = 4.0;

/// One node placed on the graph canvas.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorkflowNode {
    /// The task id (the stable selection key across refreshes).
    pub id: Uuid,
    /// Longest distance from a graph root.
    pub column: usize,
    /// Position within the column.
    pub row: usize,
    /// Left edge of the node box.
    pub x: f32,
    /// Top edge of the node box.
    pub y: f32,
}

/// One drawn lineage edge: a parent instance to the instance it started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkflowEdge {
    /// The predecessor (`parent_id`).
    pub parent: Uuid,
    /// The instance that waited for or was spawned by the parent.
    pub child: Uuid,
}

/// The paint geometry of one lineage edge.
///
/// The stroke is a cubic from the parent's right-center to the arrowhead's
/// base, with its first control point level with the parent and its second
/// level with the child. That keeps both ends horizontal: the fork reads left
/// to right and the filled head can point straight at the child.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgeGeometry {
    /// Start point on the parent's right edge.
    pub start: (f32, f32),
    /// First cubic control point, level with the parent.
    pub control_start: (f32, f32),
    /// Second cubic control point, level with the child.
    pub control_end: (f32, f32),
    /// Where the stroke stops (the arrowhead's base center).
    pub stroke_end: (f32, f32),
    /// The arrow tip, touching the child's left edge.
    pub tip: (f32, f32),
    /// First base corner of the arrowhead.
    pub base_top: (f32, f32),
    /// Second base corner of the arrowhead.
    pub base_bottom: (f32, f32),
}

/// Compute the stroke and arrowhead geometry for a parent → child edge.
///
/// The tip sits on the child's left edge at its vertical center; the base is
/// [`ARROW_LENGTH`] behind it and [`ARROW_HALF_WIDTH`] above and below, so the
/// head never covers the card's text or status chips.
pub fn edge_geometry(parent: &WorkflowNode, child: &WorkflowNode) -> EdgeGeometry {
    let start = (parent.x + NODE_WIDTH, parent.y + NODE_HEIGHT / 2.0);
    let tip = (child.x, child.y + NODE_HEIGHT / 2.0);
    // Neighbouring columns are at least COLUMN_GAP apart; clamping keeps the
    // base on the parent side even if a future layout ever gets tighter.
    let base_x = (tip.0 - ARROW_LENGTH).max(start.0);
    let stroke_end = (base_x, tip.1);
    let half_horizontal = ((stroke_end.0 - start.0) / 2.0).max(0.0);
    EdgeGeometry {
        start,
        control_start: (start.0 + half_horizontal, start.1),
        control_end: (stroke_end.0 - half_horizontal, stroke_end.1),
        stroke_end,
        tip,
        base_top: (base_x, tip.1 - ARROW_HALF_WIDTH),
        base_bottom: (base_x, tip.1 + ARROW_HALF_WIDTH),
    }
}

/// A window-free layout of one root's instances.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WorkflowGraphLayout {
    /// Placed nodes in column/row order.
    pub nodes: Vec<WorkflowNode>,
    /// Drawn edges, in child column/row order.
    pub edges: Vec<WorkflowEdge>,
    /// The full canvas width, including margins.
    pub width: f32,
    /// The full canvas height, including margins.
    pub height: f32,
}

impl WorkflowGraphLayout {
    /// The placed box for one task id.
    pub fn node(&self, id: Uuid) -> Option<&WorkflowNode> {
        self.nodes.iter().find(|node| node.id == id)
    }

    /// The parent drawn for one child, when it has a valid edge (tests assert
    /// the edge derivation; the view paints `edges` directly).
    #[cfg(test)]
    pub fn parent_of(&self, id: Uuid) -> Option<Uuid> {
        self.edges
            .iter()
            .find(|edge| edge.child == id)
            .map(|edge| edge.parent)
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
    /// The instance that spawned or unblocked this one, from the cached task
    /// snapshot. `workflow.inspect` omits it; the snapshot usually names it.
    pub parent_id: Option<Uuid>,
    /// Creation time, the graph's stable sibling order. Missing when the task
    /// is no longer in the shared snapshot.
    pub created_at: Option<DateTime<Utc>>,
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

    /// A pending instance whose `needs` predecessor has not finished yet. It
    /// ranks below an unblocked pending instance in the copy and wears a
    /// dashed outline.
    pub fn is_blocked(&self) -> bool {
        self.blocked && self.status == TaskStatus::Pending
    }

    /// Running or awaiting input: the states that animate while live.
    pub fn is_active(&self) -> bool {
        matches!(self.status, TaskStatus::Running | TaskStatus::AwaitingInput)
    }

    /// Deterministic sibling order: creation time first, then id. A status
    /// change can never reshuffle the picture.
    fn order_key(&self) -> (i64, Uuid) {
        (
            self.created_at
                .map(|created| created.timestamp_millis())
                .unwrap_or(i64::MAX),
            self.id,
        )
    }
}

/// Lay out one root's rows as a left-to-right forest.
///
/// Every node gets a column by its longest distance from a graph root, then a
/// row inside that column. Parents that are missing from the inspect set, point
/// at the node itself or form an accidental cycle are dropped, so the result is
/// always a forest and layout can never hang. The runtime data makes cycles
/// impossible in practice; this is a defensive guard, not a feature.
pub fn layout_graph(rows: &[WorkflowRow]) -> WorkflowGraphLayout {
    if rows.is_empty() {
        return WorkflowGraphLayout::default();
    }

    let mut ordered: Vec<&WorkflowRow> = rows.iter().collect();
    ordered.sort_by_key(|row| row.order_key());
    let ids: HashSet<Uuid> = ordered.iter().map(|row| row.id).collect();

    // Only a known, non-self parent can produce an edge.
    let mut parent_of: HashMap<Uuid, Uuid> = ordered
        .iter()
        .filter_map(|row| {
            row.parent_id
                .filter(|parent| *parent != row.id && ids.contains(parent))
                .map(|parent| (row.id, parent))
        })
        .collect();

    // Break accidental cycles deterministically. For a two-node loop this
    // drops the first-processed node's edge and keeps the other, leaving a
    // valid forest either way.
    for row in &ordered {
        if !parent_of.contains_key(&row.id) {
            continue;
        }
        let mut seen = HashSet::new();
        let mut current = row.id;
        loop {
            if !seen.insert(current) {
                parent_of.remove(&row.id);
                break;
            }
            match parent_of.get(&current).copied() {
                Some(parent) if parent == row.id => {
                    parent_of.remove(&row.id);
                    break;
                }
                Some(parent) => current = parent,
                None => break,
            }
        }
    }

    // Longest distance from a root. The cycle guard above bounds this walk.
    let depth = |id: Uuid| -> usize {
        let mut depth = 0usize;
        let mut current = id;
        while let Some(parent) = parent_of.get(&current).copied() {
            depth += 1;
            current = parent;
            if depth > rows.len() {
                break;
            }
        }
        depth
    };

    let mut columns: Vec<Vec<&WorkflowRow>> = Vec::new();
    for row in &ordered {
        let column = depth(row.id);
        if columns.len() <= column {
            columns.resize_with(column + 1, Vec::new);
        }
        columns[column].push(*row);
    }

    let mut nodes = Vec::with_capacity(rows.len());
    for (column, column_rows) in columns.iter().enumerate() {
        for (row, task) in column_rows.iter().enumerate() {
            nodes.push(WorkflowNode {
                id: task.id,
                column,
                row,
                x: GRAPH_MARGIN + column as f32 * (NODE_WIDTH + COLUMN_GAP),
                y: GRAPH_MARGIN + row as f32 * (NODE_HEIGHT + ROW_GAP),
            });
        }
    }

    let edges: Vec<WorkflowEdge> = nodes
        .iter()
        .filter_map(|node| {
            parent_of.get(&node.id).map(|parent| WorkflowEdge {
                parent: *parent,
                child: node.id,
            })
        })
        .collect();

    let column_count = columns.len() as f32;
    let row_count = columns.iter().map(Vec::len).max().unwrap_or(0) as f32;
    WorkflowGraphLayout {
        nodes,
        edges,
        width: GRAPH_MARGIN * 2.0
            + column_count * NODE_WIDTH
            + (column_count - 1.0).max(0.0) * COLUMN_GAP,
        height: GRAPH_MARGIN * 2.0 + row_count * NODE_HEIGHT + (row_count - 1.0).max(0.0) * ROW_GAP,
    }
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
    /// Spawned children that failed before their agent started under a
    /// succeeded parent (the persistent launch-failure warning).
    pub launch_failures: usize,
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
    /// snapshot fills the edges and the sibling order. `selected` is kept when
    /// that instance is still present, otherwise the first node in graph order
    /// is highlighted.
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
                let cached_task = cached.get(&task.id).copied();
                WorkflowRow {
                    id: task.id,
                    name: names
                        .get(&task.id)
                        .cloned()
                        .unwrap_or_else(|| inbox::task_display_name(&task.name).to_string()),
                    status: task.status,
                    attempt: task.attempt,
                    summary: task.summary.clone(),
                    parent_id: cached_task.and_then(|task| task.parent_id),
                    created_at: cached_task.map(|task| task.created_at),
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

    /// The pure layout of this root's instances.
    pub fn layout(&self) -> WorkflowGraphLayout {
        layout_graph(&self.rows)
    }

    /// The rows in graph order (column, then row).
    pub fn ordered_rows(&self) -> Vec<&WorkflowRow> {
        let layout = self.layout();
        let by_id: HashMap<Uuid, &WorkflowRow> =
            self.rows.iter().map(|row| (row.id, row)).collect();
        layout
            .nodes
            .iter()
            .filter_map(|node| by_id.get(&node.id).copied())
            .collect()
    }

    /// The highlighted row, falling back to the first row in graph order.
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

    /// Move the highlight by one in graph order, clamped (lists do not wrap).
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
            || row.parent_id != task.parent_id
            || row.created_at != Some(task.created_at)
            || row.retry_pending;
        row.status = task.status;
        row.attempt = task.attempt;
        row.summary = task.error.clone();
        row.parent_id = task.parent_id;
        row.created_at = Some(task.created_at);
        if task.status != TaskStatus::Pending {
            row.blocked = false;
        }
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
                row.blocked = false;
            }
        }
        self.pending_cancel = false;
        self.recompute_state();
    }

    /// Optimistically move one failed row back to pending. Returns the prior
    /// row so a failed RPC can restore exactly that state.
    pub fn begin_retry(&mut self, id: Uuid) -> Option<WorkflowRow> {
        let row = self.rows.iter_mut().find(|row| row.id == id)?;
        if row.status != TaskStatus::Failed || row.retry_pending {
            return None;
        }
        let previous = row.clone();
        row.status = TaskStatus::Pending;
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
    let mut launch_counts: HashMap<Uuid, usize> = HashMap::new();
    for failure in launch::launch_failures(tasks) {
        *launch_counts.entry(failure.root_id).or_insert(0) += 1;
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
                    parent_id: task.parent_id,
                    created_at: Some(task.created_at),
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
                launch_failures: launch_counts.get(&root_id).copied().unwrap_or(0),
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
            agent: None,
            provider: None,
            model: None,
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

    fn inspect_with(
        root: Uuid,
        root_name: String,
        views: Vec<favetto_wire::workflow::WorkflowTaskView>,
        blocked: Vec<Uuid>,
    ) -> WorkflowInspect {
        WorkflowInspect {
            root_id: root,
            root_task: root_name,
            state: WorkflowState::Running,
            tasks: views,
            ready: vec![],
            running: vec![],
            failed: vec![],
            blocked,
        }
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
    fn roots_count_launch_failures_under_the_succeeded_parent() {
        use favetto_wire::model::{Failure, FailureKind};

        let (root, mut parent, mut child) = two_step();
        parent.finished_at = Some(at(0));
        child.status = TaskStatus::Failed;
        child.created_at = at(1);
        child.error = Some("task not found in the catalog".to_string());
        child.failure = Some(Failure::new(
            FailureKind::InvalidInput,
            "task not found in the catalog",
        ));
        child.parent_id = Some(root);

        let roots = roots(&[parent, child]);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].launch_failures, 1);
        assert_eq!(roots[0].failed, 1);
    }

    #[test]
    fn inspect_derives_edges_blocked_markers_and_graph_order() {
        let (root, mut root_task, mut child) = two_step();
        root_task.status = TaskStatus::Failed;
        child.status = TaskStatus::Pending;
        child.parent_id = Some(root);

        let inspect = inspect_with(
            root,
            root_task.name.clone(),
            vec![
                view(
                    root_task.id,
                    &root_task.name,
                    TaskStatus::Failed,
                    2,
                    Some("boom"),
                ),
                view(child.id, &child.name, TaskStatus::Pending, 1, None),
            ],
            vec![child.id],
        );

        let detail =
            WorkflowDetail::from_inspect(&inspect, &[root_task.clone(), child.clone()], None);
        assert_eq!(detail.root_id, root);
        assert_eq!(detail.root_name, "build");
        assert_eq!(detail.total(), 2);
        assert_eq!(detail.done(), 0);
        assert_eq!(detail.cancellable(), 1);
        assert_eq!(detail.selected_id(), Some(root_task.id));

        let child_row = detail
            .rows
            .iter()
            .find(|row| row.id == Uuid::from_u128(2))
            .unwrap();
        assert_eq!(child_row.parent_id, Some(root));
        assert!(child_row.is_blocked());
        assert!(!child_row.is_active());

        let layout = detail.layout();
        assert_eq!(layout.nodes.len(), 2);
        assert_eq!(layout.node(root).unwrap().column, 0);
        assert_eq!(layout.node(child.id).unwrap().column, 1);
        assert_eq!(layout.edges.len(), 1);
        assert_eq!(layout.edges[0].parent, root);
        assert_eq!(layout.edges[0].child, child.id);
        assert!(layout.width > NODE_WIDTH);
        assert!(layout.height > NODE_HEIGHT);

        let ordered: Vec<Uuid> = detail.ordered_rows().iter().map(|row| row.id).collect();
        assert_eq!(ordered, vec![root, child.id]);
    }

    #[test]
    fn layout_is_deterministic_and_orders_siblings_by_creation_time() {
        let root = Uuid::from_u128(1);
        let mut root_task = task(1, "root", TaskStatus::Running, None);
        root_task.created_at = at(0);
        let mut later = task(2, "later", TaskStatus::Pending, Some(root));
        later.parent_id = Some(root);
        later.created_at = at(20);
        let mut earlier = task(3, "earlier", TaskStatus::Pending, Some(root));
        earlier.parent_id = Some(root);
        earlier.created_at = at(10);

        let inspect = inspect_with(
            root,
            root_task.name.clone(),
            vec![
                view(root, "root", TaskStatus::Running, 1, None),
                view(later.id, "later", TaskStatus::Pending, 1, None),
                view(earlier.id, "earlier", TaskStatus::Pending, 1, None),
            ],
            vec![],
        );
        let detail = WorkflowDetail::from_inspect(
            &inspect,
            &[root_task, later.clone(), earlier.clone()],
            None,
        );

        let first = detail.layout();
        let second = detail.layout();
        assert_eq!(first, second);
        let column_one: Vec<Uuid> = first
            .nodes
            .iter()
            .filter(|node| node.column == 1)
            .map(|node| node.id)
            .collect();
        assert_eq!(column_one, vec![earlier.id, later.id]);
        assert_eq!(first.node(earlier.id).unwrap().row, 0);
        assert_eq!(first.node(later.id).unwrap().row, 1);

        // Moving the selection walks the drawn order, not the inspect order.
        let mut detail = detail;
        detail.move_selection(1);
        assert_eq!(detail.selected_id(), Some(earlier.id));
    }

    #[test]
    fn layout_treats_missing_and_self_parents_as_graph_roots() {
        let root = Uuid::from_u128(1);
        let mut self_parent = task(2, "loop", TaskStatus::Pending, Some(root));
        self_parent.parent_id = Some(self_parent.id);
        let mut orphan = task(3, "orphan", TaskStatus::Pending, Some(root));
        orphan.parent_id = Some(Uuid::from_u128(99));
        let root_task = task(1, "root", TaskStatus::Running, None);

        let inspect = inspect_with(
            root,
            root_task.name.clone(),
            vec![
                view(root, "root", TaskStatus::Running, 1, None),
                view(self_parent.id, "loop", TaskStatus::Pending, 1, None),
                view(orphan.id, "orphan", TaskStatus::Pending, 1, None),
            ],
            vec![],
        );
        let detail =
            WorkflowDetail::from_inspect(&inspect, &[root_task, self_parent, orphan], None);
        let layout = detail.layout();
        assert_eq!(layout.edges.len(), 0);
        assert!(layout.nodes.iter().all(|node| node.column == 0));
    }

    #[test]
    fn layout_breaks_an_accidental_cycle_without_hanging() {
        let root = Uuid::from_u128(1);
        let mut left = task(2, "left", TaskStatus::Pending, Some(root));
        let mut right = task(3, "right", TaskStatus::Pending, Some(root));
        left.parent_id = Some(right.id);
        right.parent_id = Some(left.id);
        let root_task = task(1, "root", TaskStatus::Running, None);

        let inspect = inspect_with(
            root,
            root_task.name.clone(),
            vec![
                view(root, "root", TaskStatus::Running, 1, None),
                view(left.id, "left", TaskStatus::Pending, 1, None),
                view(right.id, "right", TaskStatus::Pending, 1, None),
            ],
            vec![],
        );
        let detail = WorkflowDetail::from_inspect(&inspect, &[root_task, left, right], None);
        let layout = detail.layout();
        assert_eq!(layout.nodes.len(), 3);
        assert_eq!(layout.edges.len(), 1);
        assert!(layout.nodes.iter().all(|node| node.column <= 1));
    }

    #[test]
    fn layout_places_fan_out_nodes_in_distinct_rows_without_overlap() {
        let root = Uuid::from_u128(1);
        let root_task = task(1, "root", TaskStatus::Running, None);
        let mut children: Vec<Task> = Vec::new();
        let mut views = vec![view(root, "root", TaskStatus::Running, 1, None)];
        for index in 0..3u128 {
            let id = Uuid::from_u128(10 + index);
            let mut child = task(10 + index, "child", TaskStatus::Pending, Some(root));
            child.parent_id = Some(root);
            child.created_at = at(index as i64 * 5);
            views.push(view(id, "child", TaskStatus::Pending, 1, None));
            children.push(child);
        }
        let inspect = inspect_with(root, root_task.name.clone(), views, vec![]);
        let mut all = vec![root_task];
        all.extend(children);
        let detail = WorkflowDetail::from_inspect(&inspect, &all, None);
        let layout = detail.layout();

        let mut boxes: Vec<(f32, f32)> = layout.nodes.iter().map(|node| (node.x, node.y)).collect();
        boxes.sort_by(|a, b| a.partial_cmp(b).unwrap());
        boxes.dedup();
        assert_eq!(boxes.len(), layout.nodes.len());
        assert!(layout.height >= GRAPH_MARGIN * 2.0 + NODE_HEIGHT * 3.0);
    }

    #[test]
    fn edge_geometry_puts_a_symmetric_head_on_the_child_left_edge() {
        let parent = WorkflowNode {
            id: Uuid::from_u128(1),
            column: 0,
            row: 0,
            x: GRAPH_MARGIN,
            y: GRAPH_MARGIN,
        };
        let child = WorkflowNode {
            id: Uuid::from_u128(2),
            column: 1,
            row: 2,
            x: GRAPH_MARGIN + NODE_WIDTH + COLUMN_GAP,
            y: GRAPH_MARGIN + 2.0 * (NODE_HEIGHT + ROW_GAP),
        };
        let geometry = edge_geometry(&parent, &child);

        assert_eq!(
            geometry.start,
            (parent.x + NODE_WIDTH, parent.y + NODE_HEIGHT / 2.0)
        );
        assert_eq!(geometry.tip, (child.x, child.y + NODE_HEIGHT / 2.0));
        // The stroke stops at the arrowhead base; the tip touches the card.
        assert_eq!(geometry.stroke_end.0, geometry.tip.0 - ARROW_LENGTH);
        assert_eq!(geometry.stroke_end.1, geometry.tip.1);
        // The base is symmetric around the child's vertical center.
        assert_eq!(geometry.base_top.1, geometry.tip.1 - ARROW_HALF_WIDTH);
        assert_eq!(geometry.base_bottom.1, geometry.tip.1 + ARROW_HALF_WIDTH);
        assert_eq!(
            geometry.base_top.1 + geometry.base_bottom.1,
            geometry.tip.1 * 2.0
        );
        assert_eq!(geometry.base_top.0, geometry.base_bottom.0);

        // Both cubic ends are horizontal, so the head points right.
        assert_eq!(geometry.control_start.1, geometry.start.1);
        assert_eq!(geometry.control_end.1, geometry.stroke_end.1);
        assert!(geometry.stroke_end.0 > geometry.start.0);
        assert!(geometry.stroke_end.0 < geometry.tip.0);
    }

    #[test]
    fn empty_inspect_has_zero_progress_and_no_selection() {
        let root = Uuid::from_u128(3);
        let inspect = inspect_with(root, "empty".to_string(), vec![], vec![]);
        let detail = WorkflowDetail::from_inspect(&inspect, &[], None);
        assert_eq!(detail.progress(), 0.0);
        assert_eq!(detail.selected_id(), None);
        assert_eq!(detail.cancellable(), 0);
        assert_eq!(detail.layout(), WorkflowGraphLayout::default());
    }

    #[test]
    fn cancel_moves_every_non_terminal_row_to_cancelled() {
        let (root, mut root_task, mut child) = two_step();
        root_task.status = TaskStatus::Running;
        child.status = TaskStatus::Pending;
        let inspect = inspect_with(
            root,
            root_task.name.clone(),
            vec![
                view(root_task.id, &root_task.name, TaskStatus::Running, 1, None),
                view(child.id, &child.name, TaskStatus::Pending, 1, None),
            ],
            vec![],
        );
        let mut detail = WorkflowDetail::from_inspect(&inspect, &[root_task, child], None);
        assert_eq!(detail.cancellable(), 2);
        assert_eq!(detail.begin_cancel(), 2);
        assert!(detail.pending_cancel);
        assert_eq!(detail.cancellable(), 0);
        assert!(detail
            .rows
            .iter()
            .all(|row| row.status == TaskStatus::Cancelled));
        assert_eq!(detail.state, WorkflowState::Cancelled);
        detail.finish_cancel(&[Uuid::from_u128(1), Uuid::from_u128(2)]);
        assert!(!detail.pending_cancel);
        assert_eq!(detail.state, WorkflowState::Cancelled);
    }

    #[test]
    fn retry_moves_a_failed_row_to_pending_and_reflects_the_new_attempt() {
        let (root, mut root_task, mut child) = two_step();
        root_task.status = TaskStatus::Failed;
        root_task.attempt = 1;
        child.status = TaskStatus::Pending;

        let inspect = inspect_with(
            root,
            root_task.name.clone(),
            vec![
                view(
                    root_task.id,
                    &root_task.name,
                    TaskStatus::Failed,
                    1,
                    Some("boom"),
                ),
                view(child.id, &child.name, TaskStatus::Pending, 1, None),
            ],
            vec![],
        );
        let mut detail = WorkflowDetail::from_inspect(&inspect, &[root_task.clone(), child], None);
        detail.begin_retry(root_task.id).expect("retryable row");
        // The selection is stable by id even though the failed row is now
        // pending and the graph may have re-ordered.
        assert_eq!(detail.selected_id(), Some(root_task.id));
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
        assert_eq!(row.status_kind(), StatusKind::Failed);
    }

    #[test]
    fn apply_task_moves_the_row_and_keeps_the_selection() {
        let (root, root_task, child) = two_step();
        let inspect = inspect_with(
            root,
            root_task.name.clone(),
            vec![
                view(
                    root_task.id,
                    &root_task.name,
                    TaskStatus::Succeeded,
                    1,
                    None,
                ),
                view(child.id, &child.name, TaskStatus::Pending, 1, None),
            ],
            vec![],
        );
        let mut detail =
            WorkflowDetail::from_inspect(&inspect, &[root_task, child], Some(Uuid::from_u128(2)));
        assert_eq!(detail.selected_id(), Some(Uuid::from_u128(2)));
        let mut running_child = task(2, "repo:demo/test", TaskStatus::Running, Some(root));
        running_child.attempt = 3;
        running_child.parent_id = Some(root);
        assert!(detail.apply_task(&running_child));
        assert_eq!(detail.selected_id(), Some(Uuid::from_u128(2)));
        let row = detail
            .rows
            .iter()
            .find(|row| row.id == running_child.id)
            .unwrap();
        assert_eq!(row.attempt, 3);
        assert!(row.is_active());
        assert_eq!(detail.layout().parent_of(row.id), Some(root));
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
