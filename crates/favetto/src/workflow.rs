//! Derive the task catalog's workflow graph from `needs`/`spawn` as Graphviz DOT.
//!
//! Nodes are catalog tasks. Edges: `spawn = "child"` → parent→child (solid,
//! labelled `spawn`); `needs = "other:finished"` → other→this (dashed, labelled
//! `needs`, source resolved by stripping the suffix after `:`);
//! `needs = "other:all_finished"` → other→this (dashed, labelled `join`) as a
//! distinct fan-in edge. Isolated tasks are still nodes; references to names
//! absent from the catalog become dashed "external" nodes; `schedule` marks a
//! node with `peripheries=2`.
//!
//! `build_dot` is pure and deterministic: task order does not affect the output.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
/// `needs = "other:finished"` is [`Needs`](Self::Needs), and the root-scoped
/// fan-in `needs = "other:all_finished"` is [`Join`](Self::Join).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowEdgeKind {
    Spawn,
    Needs,
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
}
