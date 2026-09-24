//! Structured workflow graph and runtime-inspect value types.
//!
//! The Graphviz DOT builders that derive these from the Markdown catalog live in
//! `favetto-core`; this module owns the pure wire types both the daemon and the
//! TUI share.
//!
//! Nodes are catalog tasks. Edges: `spawn = "child"` → parent→child (solid,
//! labelled `spawn`); `needs = "other:finished"` → other→this (dashed, labelled
//! `needs`, source resolved by stripping the suffix after `:`); the outcome
//! conditions `other:succeeded` / `other:failed` are dashed edges labelled
//! `needs:succeeded` / `needs:failed`; `needs = "other:all_finished"` →
//! other→this (dashed, labelled `join`) as a distinct fan-in edge. Isolated
//! tasks are still nodes; references to names absent from the catalog become
//! dashed "external" nodes; `schedule` marks a node with `peripheries=2`.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::TaskStatus;

/// One node of the structured workflow graph: a catalog task or an `external`
/// reference to a name absent from the catalog. `scheduled` mirrors
/// `TaskDef::schedule`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkflowNode {
    pub name: String,
    #[serde(default)]
    pub scheduled: bool,
    #[serde(default)]
    pub external: bool,
}

/// The edge kinds: `spawn = "child"` is [`Spawn`](Self::Spawn),
/// `needs = "other:finished"` (or its alias `:terminal`) is
/// [`Needs`](Self::Needs), `needs = "other:succeeded"` is
/// [`NeedsSucceeded`](Self::NeedsSucceeded), `needs = "other:failed"` is
/// [`NeedsFailed`](Self::NeedsFailed), and the root-scoped fan-in
/// `needs = "other:all_finished"` is [`Join`](Self::Join).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WorkflowEdgeKind {
    Spawn,
    Needs,
    NeedsSucceeded,
    NeedsFailed,
    Join,
}

/// One directed edge of the workflow graph (`from -> to`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkflowEdge {
    pub from: String,
    pub to: String,
    pub kind: WorkflowEdgeKind,
}

/// Structured, deterministic counterpart of [`build_dot`] — the graph the TUI
/// lays out instead of parsing DOT.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkflowGraph {
    pub nodes: Vec<WorkflowNode>,
    pub edges: Vec<WorkflowEdge>,
}

/// One task instance in the runtime workflow view (`workflow.inspect`).
///
/// Deliberately excludes `output` and every other blob: per-task detail stays
/// behind `tasks.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkflowTaskView {
    pub id: Uuid,
    pub name: String,
    pub status: TaskStatus,
    /// Run attempt. Always `1` until task runs land (#147 advances it).
    #[serde(default = "default_attempt")]
    pub attempt: u32,
    /// Bounded outcome text: `Task::error` for failures/cancellations, `None`
    /// for successes until the result envelope (#144) provides one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

fn default_attempt() -> u32 {
    1
}

/// Overall state of a workflow root in the runtime view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WorkflowState {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

/// The runtime graph returned by `workflow.inspect`: the root's task instances
/// plus id buckets. `ready`/`blocked` are the `Pending` split by whether a
/// `needs` predecessor is still active.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkflowInspect {
    pub root_id: Uuid,
    pub root_task: String,
    pub state: WorkflowState,
    pub tasks: Vec<WorkflowTaskView>,
    pub ready: Vec<Uuid>,
    pub running: Vec<Uuid>,
    pub failed: Vec<Uuid>,
    pub blocked: Vec<Uuid>,
}

/// One node created by `workflow.create`: the caller's local `key` plus the id
/// and catalog name assigned to the new (or already-existing) task instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkflowNodeRef {
    /// The caller's local key, used by other nodes' `depends_on`.
    pub key: String,
    pub id: Uuid,
    pub name: String,
}

/// The result of `workflow.create`: the root the DAG belongs to and one
/// [`WorkflowNodeRef`] per submitted task, in request order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkflowCreateResult {
    pub root_id: Uuid,
    pub tasks: Vec<WorkflowNodeRef>,
}

/// The result of `workflow.cancel`: the root that was targeted and the ids of
/// every task actually transitioned to `cancelled`, in root order.
///
/// Tasks that were already terminal are omitted, so `cancelled.len()` is the
/// number of `TaskCancelled` events emitted for this request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkflowCancelResult {
    pub root_id: Uuid,
    pub cancelled: Vec<Uuid>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_kind_wire_names_are_stable() {
        for (kind, wire) in [
            (WorkflowEdgeKind::Spawn, "spawn"),
            (WorkflowEdgeKind::Needs, "needs"),
            (WorkflowEdgeKind::NeedsSucceeded, "needs_succeeded"),
            (WorkflowEdgeKind::NeedsFailed, "needs_failed"),
            (WorkflowEdgeKind::Join, "join"),
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), serde_json::json!(wire));
            assert_eq!(
                serde_json::from_value::<WorkflowEdgeKind>(serde_json::json!(wire)).unwrap(),
                kind
            );
        }
    }

    #[test]
    fn inspect_view_round_trips_with_snake_case_and_omitted_summary() {
        let root_id = Uuid::new_v4();
        let running_id = Uuid::new_v4();
        let view = WorkflowInspect {
            root_id,
            root_task: "root".to_string(),
            state: WorkflowState::Running,
            tasks: vec![
                WorkflowTaskView {
                    id: running_id,
                    name: "child".to_string(),
                    status: TaskStatus::AwaitingInput,
                    attempt: 1,
                    summary: None,
                },
                WorkflowTaskView {
                    id: Uuid::new_v4(),
                    name: "child".to_string(),
                    status: TaskStatus::Failed,
                    attempt: 1,
                    summary: Some("boom".to_string()),
                },
            ],
            ready: vec![],
            running: vec![running_id],
            failed: vec![],
            blocked: vec![],
        };

        let value = serde_json::to_value(&view).unwrap();
        assert_eq!(value["state"], "running");
        assert_eq!(value["tasks"][0]["status"], "awaiting_input");
        assert_eq!(value["tasks"][0]["attempt"], 1);
        // A missing summary is omitted, not serialized as null.
        assert!(value["tasks"][0].get("summary").is_none());
        assert_eq!(value["tasks"][1]["summary"], "boom");

        let decoded: WorkflowInspect = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, view);
    }

    #[test]
    fn create_result_round_trips() {
        let root_id = Uuid::new_v4();
        let result = WorkflowCreateResult {
            root_id,
            tasks: vec![
                WorkflowNodeRef {
                    key: "a".to_string(),
                    id: Uuid::new_v4(),
                    name: "build".to_string(),
                },
                WorkflowNodeRef {
                    key: "b".to_string(),
                    id: Uuid::new_v4(),
                    name: "test".to_string(),
                },
            ],
        };

        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(value["root_id"], root_id.to_string());
        assert_eq!(value["tasks"][0]["key"], "a");
        assert_eq!(value["tasks"][0]["name"], "build");

        let decoded: WorkflowCreateResult = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, result);
    }

    #[test]
    fn cancel_result_round_trips() {
        let root_id = Uuid::new_v4();
        let cancelled = vec![Uuid::new_v4(), Uuid::new_v4()];
        let result = WorkflowCancelResult {
            root_id,
            cancelled: cancelled.clone(),
        };

        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(value["root_id"], root_id.to_string());
        assert_eq!(value["cancelled"][0], cancelled[0].to_string());

        let decoded: WorkflowCancelResult = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, result);
    }
}
