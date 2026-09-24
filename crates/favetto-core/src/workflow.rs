//! Derive the task catalog's workflow graph from `needs`/`spawn` as Graphviz DOT.
//!
//! Nodes are catalog tasks. Edges: `spawn = "child"` → parent→child (solid,
//! labelled `spawn`); `needs = "other:finished"` → other→this (dashed, labelled
//! `needs`, source resolved by stripping the suffix after `:`); the outcome
//! conditions `other:succeeded` / `other:failed` are dashed edges labelled
//! `needs:succeeded` / `needs:failed`; `needs = "other:all_finished"` →
//! other→this (dashed, labelled `join`) as a distinct fan-in edge. Isolated
//! tasks are still nodes; references to names absent from the catalog become
//! dashed "external" nodes; `schedule` marks a node with `peripheries=2`.
//!
//! `build_dot` is pure and deterministic: task order does not affect the output.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::TaskStatus;
use crate::tasks::{needs_parts, NeedsKind, TaskDef};

/// Escape a name/label for inclusion in a quoted DOT id or label.
fn dot_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// One node of the structured workflow graph: a catalog task or an `external`
/// reference to a name absent from the catalog. `scheduled` mirrors
/// `TaskDef::schedule`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
pub struct WorkflowEdge {
    pub from: String,
    pub to: String,
    pub kind: WorkflowEdgeKind,
}

/// Structured, deterministic counterpart of [`build_dot`] — the graph the TUI
/// lays out instead of parsing DOT.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowGraph {
    pub nodes: Vec<WorkflowNode>,
    pub edges: Vec<WorkflowEdge>,
}

/// One task instance in the runtime workflow view (`workflow.inspect`).
///
/// Deliberately excludes `output` and every other blob: per-task detail stays
/// behind `tasks.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
pub struct WorkflowNodeRef {
    /// The caller's local key, used by other nodes' `depends_on`.
    pub key: String,
    pub id: Uuid,
    pub name: String,
}

/// The result of `workflow.create`: the root the DAG belongs to and one
/// [`WorkflowNodeRef`] per submitted task, in request order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowCreateResult {
    pub root_id: Uuid,
    pub tasks: Vec<WorkflowNodeRef>,
}

/// Build the catalog's `needs`/`spawn` graph as structured data.
///
/// Deterministic regardless of the order of `tasks`: catalog nodes are sorted by
/// name, external nodes follow sorted by name, and each catalog task's `spawn`
/// edge is emitted before its `needs` edge.
pub fn build_graph(tasks: &[TaskDef]) -> WorkflowGraph {
    let mut sorted: Vec<&TaskDef> = tasks.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));

    let names: BTreeSet<&str> = tasks.iter().map(|d| d.name.as_str()).collect();

    // Catalog task nodes, sorted by name. Scheduled tasks are marked.
    let mut nodes: Vec<WorkflowNode> = sorted
        .iter()
        .map(|def| WorkflowNode {
            name: def.name.clone(),
            scheduled: def.schedule.is_some(),
            external: false,
        })
        .collect();

    // External nodes: `spawn` targets and resolved `needs` sources that are not
    // in the catalog. Declared once each, sorted.
    let mut external: BTreeSet<&str> = BTreeSet::new();
    for def in &sorted {
        if let Some(spawn) = &def.spawn {
            if !names.contains(spawn.as_str()) {
                external.insert(spawn.as_str());
            }
        }
        if let Some(needs) = &def.needs {
            let (source, _) = needs_parts(needs);
            if !names.contains(source) {
                external.insert(source);
            }
        }
    }
    nodes.extend(external.iter().map(|name| WorkflowNode {
        name: (*name).to_string(),
        scheduled: false,
        external: true,
    }));

    // Edges, spawn first then needs, both in task-name order.
    let mut edges: Vec<WorkflowEdge> = Vec::new();
    for def in &sorted {
        if let Some(spawn) = &def.spawn {
            edges.push(WorkflowEdge {
                from: def.name.clone(),
                to: spawn.clone(),
                kind: WorkflowEdgeKind::Spawn,
            });
        }
        if let Some(needs) = &def.needs {
            let (source, kind) = needs_parts(needs);
            edges.push(WorkflowEdge {
                from: source.to_string(),
                to: def.name.clone(),
                kind: match kind {
                    NeedsKind::Finished => WorkflowEdgeKind::Needs,
                    NeedsKind::Succeeded => WorkflowEdgeKind::NeedsSucceeded,
                    NeedsKind::Failed => WorkflowEdgeKind::NeedsFailed,
                    NeedsKind::AllFinished => WorkflowEdgeKind::Join,
                },
            });
        }
    }

    WorkflowGraph { nodes, edges }
}

/// Render the catalog's `needs`/`spawn` graph as Graphviz DOT.
///
/// The output is deterministic regardless of the order of `tasks`.
pub fn build_dot(tasks: &[TaskDef]) -> String {
    let graph = build_graph(tasks);

    let mut out = String::new();
    out.push_str("digraph workflow {\n");
    out.push_str("  rankdir=LR;\n");
    out.push_str("  node [shape=box, style=rounded];\n");

    for node in &graph.nodes {
        let id = dot_escape(&node.name);
        if node.external {
            out.push_str(&format!(
                "  \"{id}\" [label=\"{id} (external)\", style=\"rounded,dashed\"];\n"
            ));
        } else if node.scheduled {
            out.push_str(&format!("  \"{id}\" [label=\"{id}\", peripheries=2];\n"));
        } else {
            out.push_str(&format!("  \"{id}\" [label=\"{id}\"];\n"));
        }
    }

    for edge in &graph.edges {
        let from = dot_escape(&edge.from);
        let to = dot_escape(&edge.to);
        match edge.kind {
            WorkflowEdgeKind::Spawn => {
                out.push_str(&format!("  \"{from}\" -> \"{to}\" [label=\"spawn\"];\n"));
            }
            WorkflowEdgeKind::Needs => {
                out.push_str(&format!(
                    "  \"{from}\" -> \"{to}\" [label=\"needs\", style=dashed];\n"
                ));
            }
            WorkflowEdgeKind::NeedsSucceeded => {
                out.push_str(&format!(
                    "  \"{from}\" -> \"{to}\" [label=\"needs:succeeded\", style=dashed];\n"
                ));
            }
            WorkflowEdgeKind::NeedsFailed => {
                out.push_str(&format!(
                    "  \"{from}\" -> \"{to}\" [label=\"needs:failed\", style=dashed];\n"
                ));
            }
            WorkflowEdgeKind::Join => {
                out.push_str(&format!(
                    "  \"{from}\" -> \"{to}\" [label=\"join\", style=dashed];\n"
                ));
            }
        }
    }

    out.push_str("}\n");
    out
}

/// Path of the persisted workflow graph: `<data_dir>/workflow.dot`.
pub fn dot_path(data_dir: &Path) -> PathBuf {
    data_dir.join("workflow.dot")
}

/// Write `contents` to `path` atomically (temp file in the same directory + rename).
///
/// A reader therefore never observes a partially-written graph. The temp name is
/// suffixed with a process-wide sequence so two concurrent regenerations cannot
/// clobber each other's temp file.
pub fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".workflow.dot.tmp-{}-{seq}", std::process::id()));
    std::fs::write(&tmp, contents)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Render `catalog` and persist it to `<data_dir>/workflow.dot`.
pub fn regenerate(catalog: &[TaskDef], data_dir: &Path) -> std::io::Result<PathBuf> {
    let path = dot_path(data_dir);
    write_atomic(&path, &build_dot(catalog))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal task with no edges; tests set the fields they care about.
    fn task(name: &str) -> TaskDef {
        TaskDef {
            name: name.to_string(),
            agent: Some("x".to_string()),
            provider: None,
            model: None,
            cwd: None,
            schedule: None,
            needs: None,
            spawn: None,
            spawn_file: None,
            spawn_new_root: false,
            sign: None,
            vars: Vec::new(),
            prompt: String::new(),
        }
    }

    /// A minimal task with the given `needs` value.
    fn needs_task(name: &str, needs: &str) -> TaskDef {
        let mut t = task(name);
        t.needs = Some(needs.to_string());
        t
    }

    /// A unique scratch directory for workflow tests.
    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "favetto-workflow-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn nodes_for_every_task_and_isolated_tasks_included() {
        let dot = build_dot(&[task("a"), task("b"), task("c")]);
        assert!(dot.contains("  \"a\" [label=\"a\"];"), "{dot}");
        assert!(dot.contains("  \"b\" [label=\"b\"];"), "{dot}");
        assert!(dot.contains("  \"c\" [label=\"c\"];"), "{dot}");
        assert!(!dot.contains("->"), "{dot}");
    }

    #[test]
    fn spawn_edge_is_solid_and_labelled() {
        let mut a = task("a");
        a.spawn = Some("b".to_string());
        let dot = build_dot(&[a, task("b")]);
        assert!(dot.contains("  \"a\" -> \"b\" [label=\"spawn\"];"), "{dot}");
    }

    #[test]
    fn needs_edge_is_dashed_and_strips_suffix() {
        let mut b = task("b");
        b.needs = Some("a:finished".to_string());
        let dot = build_dot(&[task("a"), b]);
        assert!(
            dot.contains("  \"a\" -> \"b\" [label=\"needs\", style=dashed];"),
            "{dot}"
        );
        assert!(!dot.contains(":finished"), "{dot}");
        assert!(!dot.contains("\"a:finished\""), "{dot}");
    }

    #[test]
    fn needs_without_suffix_uses_whole_value() {
        let mut b = task("b");
        b.needs = Some("a".to_string());
        let dot = build_dot(&[task("a"), b]);
        assert!(
            dot.contains("  \"a\" -> \"b\" [label=\"needs\", style=dashed];"),
            "{dot}"
        );
    }

    #[test]
    fn all_finished_needs_is_a_distinct_join_edge() {
        let mut b = task("b");
        b.needs = Some("a:all_finished".to_string());
        let dot = build_dot(&[task("a"), b.clone()]);
        assert!(
            dot.contains("  \"a\" -> \"b\" [label=\"join\", style=dashed];"),
            "{dot}"
        );
        assert!(!dot.contains(":all_finished"), "{dot}");

        // The structured graph uses the `Join` kind, not `Needs`.
        let graph = build_graph(&[task("a"), b]);
        assert_eq!(
            graph.edges,
            vec![WorkflowEdge {
                from: "a".to_string(),
                to: "b".to_string(),
                kind: WorkflowEdgeKind::Join,
            }]
        );
    }

    #[test]
    fn outcome_needs_edges_are_distinct_and_labelled() {
        let tasks = vec![
            task("a"),
            needs_task("bad", "a:failed"),
            needs_task("either", "a:terminal"),
            needs_task("ok", "a:succeeded"),
        ];

        // `:terminal` is an alias for `:finished` (`Needs`); `:succeeded` and
        // `:failed` get their own edge kinds.
        let graph = build_graph(&tasks);
        assert_eq!(
            graph.edges,
            vec![
                WorkflowEdge {
                    from: "a".to_string(),
                    to: "bad".to_string(),
                    kind: WorkflowEdgeKind::NeedsFailed,
                },
                WorkflowEdge {
                    from: "a".to_string(),
                    to: "either".to_string(),
                    kind: WorkflowEdgeKind::Needs,
                },
                WorkflowEdge {
                    from: "a".to_string(),
                    to: "ok".to_string(),
                    kind: WorkflowEdgeKind::NeedsSucceeded,
                },
            ]
        );

        let dot = build_dot(&tasks);
        assert!(
            dot.contains("  \"a\" -> \"bad\" [label=\"needs:failed\", style=dashed];"),
            "{dot}"
        );
        assert!(
            dot.contains("  \"a\" -> \"ok\" [label=\"needs:succeeded\", style=dashed];"),
            "{dot}"
        );
        assert!(
            dot.contains("  \"a\" -> \"either\" [label=\"needs\", style=dashed];"),
            "{dot}"
        );
        // The suffixes are stripped from the resolved source node names.
        assert!(!dot.contains("\"a:succeeded\""), "{dot}");
        assert!(!dot.contains("\"a:failed\""), "{dot}");
        assert!(!dot.contains("terminal"), "{dot}");
    }

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
    fn unknown_all_finished_target_is_an_external_node() {
        let mut b = task("b");
        b.needs = Some("ghost:all_finished".to_string());
        let graph = build_graph(&[b]);
        let ghost = graph.nodes.iter().find(|n| n.name == "ghost").unwrap();
        assert!(ghost.external);
        assert!(!graph.nodes.iter().any(|n| n.name.contains(':')));
    }

    #[test]
    fn unknown_spawn_and_needs_targets_are_external_dashed_nodes() {
        let mut a = task("a");
        a.spawn = Some("ghost".to_string());
        a.needs = Some("missing:finished".to_string());
        let dot = build_dot(&[a]);
        assert!(
            dot.contains("  \"ghost\" [label=\"ghost (external)\", style=\"rounded,dashed\"];"),
            "{dot}"
        );
        assert!(
            dot.contains("  \"missing\" [label=\"missing (external)\", style=\"rounded,dashed\"];"),
            "{dot}"
        );
    }

    #[test]
    fn scheduled_task_gets_a_distinct_marker() {
        let mut s = task("s");
        s.schedule = Some("0 0 8 * * *".to_string());
        let dot = build_dot(&[s, task("a")]);
        assert!(
            dot.contains("  \"s\" [label=\"s\", peripheries=2];"),
            "{dot}"
        );
        assert!(dot.contains("  \"a\" [label=\"a\"];"), "{dot}");
        assert!(
            !dot.contains("\"a\" [label=\"a\", peripheries=2];"),
            "{dot}"
        );
    }

    #[test]
    fn dot_is_deterministic_regardless_of_input_order() {
        let mut a = task("a");
        a.spawn = Some("b".to_string());
        let mut c = task("c");
        c.needs = Some("a:finished".to_string());

        let one = build_dot(&[a.clone(), task("b"), c.clone()]);
        let two = build_dot(&[c, task("b"), a]);
        assert_eq!(one, two);
    }

    #[test]
    fn build_graph_has_a_node_per_task_and_marks_scheduled() {
        let mut s = task("s");
        s.schedule = Some("0 0 8 * * *".to_string());
        let graph = build_graph(&[s, task("a"), task("b")]);

        assert_eq!(graph.nodes.len(), 3);
        assert_eq!(graph.nodes[0].name, "a");
        assert!(!graph.nodes[0].scheduled);
        assert_eq!(graph.nodes[1].name, "b");
        assert_eq!(graph.nodes[2].name, "s");
        assert!(graph.nodes[2].scheduled);
        assert!(!graph.nodes[2].external);
        // Isolated tasks are nodes with no edges.
        assert!(graph.edges.is_empty(), "{graph:?}");
    }

    #[test]
    fn build_graph_marks_external_targets() {
        let mut a = task("a");
        a.spawn = Some("ghost".to_string());
        a.needs = Some("missing:finished".to_string());
        let graph = build_graph(&[a]);

        assert_eq!(graph.nodes.len(), 3);
        assert_eq!(graph.nodes[0].name, "a");
        assert!(!graph.nodes[0].external);
        let ghost = graph.nodes.iter().find(|n| n.name == "ghost").unwrap();
        assert!(ghost.external);
        let missing = graph.nodes.iter().find(|n| n.name == "missing").unwrap();
        assert!(missing.external);
        // The `:finished` suffix is stripped from the external node name.
        assert!(!graph.nodes.iter().any(|n| n.name.contains(':')));
    }

    #[test]
    fn build_graph_edges_are_spawn_then_needs_in_name_order() {
        let mut a = task("a");
        a.spawn = Some("b".to_string());
        let mut c = task("c");
        c.needs = Some("a:finished".to_string());

        let graph = build_graph(&[c.clone(), task("b"), a.clone()]);
        let edges: Vec<_> = graph
            .edges
            .iter()
            .map(|e| (e.from.as_str(), e.to.as_str(), e.kind))
            .collect();
        assert_eq!(
            edges,
            vec![
                ("a", "b", WorkflowEdgeKind::Spawn),
                ("a", "c", WorkflowEdgeKind::Needs),
            ]
        );

        // Same graph regardless of input order.
        assert_eq!(graph, build_graph(&[a, task("b"), c]));
    }

    #[test]
    fn escapes_quotes_in_names_and_labels() {
        let dot = build_dot(&[task("we\"ird\\")]);
        assert!(dot.contains(r#""we\"ird\\" [label="we\"ird\\"];"#), "{dot}");
        // The raw, unescaped name must not survive into the output.
        assert!(!dot.contains("we\"ird"), "{dot}");
    }

    #[test]
    fn regenerate_writes_the_dot_atomically() {
        let dir = temp_dir("regen");
        let mut a = task("a");
        a.spawn = Some("b".to_string());
        let tasks = vec![a, task("b")];

        let path = regenerate(&tasks, &dir).unwrap();
        assert_eq!(path, dir.join("workflow.dot"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), build_dot(&tasks));

        // No temp files are left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".workflow.dot.tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn regenerate_creates_a_missing_data_dir() {
        let root = temp_dir("missing");
        let data_dir = root.join("nested").join("data");
        let path = regenerate(&[task("a")], &data_dir).unwrap();
        assert!(path.is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_atomic_replaces_existing_contents() {
        let dir = temp_dir("replace");
        let path = dir.join("workflow.dot");
        write_atomic(&path, "first").unwrap();
        write_atomic(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        let leftovers = std::fs::read_dir(&dir).unwrap().flatten().any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(".workflow.dot.tmp-")
        });
        assert!(!leftovers);
        let _ = std::fs::remove_dir_all(&dir);
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
}
