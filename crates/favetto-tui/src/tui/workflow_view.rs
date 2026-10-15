//! Render the structured catalog workflow graph with the pure-Rust `ascii-dag`
//! layout engine (no Graphviz binary, no DOT parsing).
//!
//! `ascii-dag` handles cycles and self-references internally (back edges are
//! reversed and dashed, self-loops get a marker), so this never panics on a
//! malformed catalog. If rendering is impossible the overlay falls back to the
//! raw DOT returned by `workflow.get`.

use std::collections::HashMap;

use ascii_dag::{BoxedNode, Graph, LabelOverflow, NodeId, RenderOptions, AUTO};

use crate::tasks::display_name;
use crate::workflow::{WorkflowEdgeKind, WorkflowGraph};

/// Render `graph` as box-drawing text (one entry per output row).
///
/// Node labels carry the `(external)`/`(scheduled)` suffixes; edge labels are
/// `spawn`/`needs`/`needs:succeeded`/`needs:failed`/`join`. Returns `Err` only
/// when an edge references an unknown node,
/// which [`crate::workflow::build_graph`] never produces — the caller then
/// falls back to raw DOT.
pub fn render(graph: &WorkflowGraph) -> Result<String, String> {
    // Labels live here so `BoxedNode` can borrow them for the graph's lifetime.
    let labels: Vec<String> = graph
        .nodes
        .iter()
        .map(|node| {
            let base = display_name(&node.name);
            if node.external {
                format!("{base} (external)")
            } else if node.scheduled {
                format!("{base} (scheduled)")
            } else {
                base.to_string()
            }
        })
        .collect();

    let mut dag: Graph<'_> = Graph::new();
    let mut ids: HashMap<&str, NodeId> = HashMap::with_capacity(graph.nodes.len());
    for (node, label) in graph.nodes.iter().zip(&labels) {
        let id = dag.add_node(AUTO, BoxedNode(label.as_str())).node;
        ids.insert(node.name.as_str(), id);
    }

    for edge in &graph.edges {
        let from = *ids
            .get(edge.from.as_str())
            .ok_or_else(|| format!("edge source not in graph: {}", edge.from))?;
        let to = *ids
            .get(edge.to.as_str())
            .ok_or_else(|| format!("edge target not in graph: {}", edge.to))?;
        let label = match edge.kind {
            WorkflowEdgeKind::Spawn => "spawn",
            WorkflowEdgeKind::Needs => "needs",
            WorkflowEdgeKind::NeedsSucceeded => "needs:succeeded",
            WorkflowEdgeKind::NeedsFailed => "needs:failed",
            WorkflowEdgeKind::Join => "join",
        };
        dag.add_edge(from, to, Some(label));
    }

    // Never silently drop a spawn/needs label: send unplaceable labels to the
    // legend and always print it.
    let mut options = RenderOptions::plain();
    options.plan.label_policy.overflow = LabelOverflow::Legend;
    options.emit.render_legend = true;
    Ok(dag.compute_layout().render_string(&options))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::{WorkflowEdge, WorkflowNode};

    fn node(name: &str) -> WorkflowNode {
        WorkflowNode {
            name: name.to_string(),
            scheduled: false,
            external: false,
        }
    }

    fn edge(from: &str, to: &str, kind: WorkflowEdgeKind) -> WorkflowEdge {
        WorkflowEdge {
            from: from.to_string(),
            to: to.to_string(),
            kind,
        }
    }

    #[test]
    fn render_draws_boxed_nodes_and_edge_labels() {
        let graph = WorkflowGraph {
            nodes: vec![node("a"), node("b")],
            edges: vec![edge("a", "b", WorkflowEdgeKind::Spawn)],
        };
        let text = render(&graph).unwrap();
        assert!(text.contains('┌'), "{text}");
        assert!(text.contains("a"), "{text}");
        assert!(text.contains("b"), "{text}");
        assert!(text.contains("spawn"), "{text}");
        assert!(!text.contains("digraph"), "{text}");
    }

    #[test]
    fn render_marks_external_and_scheduled() {
        let graph = WorkflowGraph {
            nodes: vec![
                WorkflowNode {
                    name: "s".to_string(),
                    scheduled: true,
                    external: false,
                },
                WorkflowNode {
                    name: "ghost".to_string(),
                    scheduled: false,
                    external: true,
                },
            ],
            edges: vec![edge("ghost", "s", WorkflowEdgeKind::Needs)],
        };
        let text = render(&graph).unwrap();
        assert!(text.contains("(scheduled)"), "{text}");
        assert!(text.contains("(external)"), "{text}");
        assert!(text.contains("needs"), "{text}");
    }

    #[test]
    fn render_labels_join_edge() {
        let graph = WorkflowGraph {
            nodes: vec![node("a"), node("b")],
            edges: vec![edge("a", "b", WorkflowEdgeKind::Join)],
        };
        let text = render(&graph).unwrap();
        assert!(text.contains("join"), "{text}");
    }

    #[test]
    fn render_labels_outcome_edges() {
        let graph = WorkflowGraph {
            nodes: vec![node("a"), node("ok"), node("bad")],
            edges: vec![
                edge("a", "ok", WorkflowEdgeKind::NeedsSucceeded),
                edge("a", "bad", WorkflowEdgeKind::NeedsFailed),
            ],
        };
        let text = render(&graph).unwrap();
        assert!(text.contains("needs:succeeded"), "{text}");
        assert!(text.contains("needs:failed"), "{text}");
    }

    #[test]
    fn render_includes_isolated_node() {
        let graph = WorkflowGraph {
            nodes: vec![node("lonely")],
            edges: Vec::new(),
        };
        let text = render(&graph).unwrap();
        assert!(text.contains("lonely"), "{text}");
    }

    #[test]
    fn render_handles_cycle_without_panic() {
        let graph = WorkflowGraph {
            nodes: vec![node("a"), node("b")],
            edges: vec![
                edge("a", "b", WorkflowEdgeKind::Spawn),
                edge("b", "a", WorkflowEdgeKind::Needs),
            ],
        };
        assert!(render(&graph).is_ok());
    }

    #[test]
    fn render_handles_self_reference() {
        let graph = WorkflowGraph {
            nodes: vec![node("a")],
            edges: vec![edge("a", "a", WorkflowEdgeKind::Needs)],
        };
        assert!(render(&graph).is_ok());
    }

    #[test]
    fn render_strips_repo_prefix_from_node_labels() {
        let graph = WorkflowGraph {
            nodes: vec![node("repo:my-repo/build"), node("repo:my-repo/test")],
            edges: vec![edge(
                "repo:my-repo/build",
                "repo:my-repo/test",
                WorkflowEdgeKind::Needs,
            )],
        };
        let text = render(&graph).unwrap();
        // The bare relative paths are shown and edges still resolve.
        assert!(text.contains("build"), "{text}");
        assert!(text.contains("test"), "{text}");
        assert!(!text.contains("repo:my-repo/build"), "{text}");
        assert!(!text.contains("repo:my-repo/test"), "{text}");
    }

    #[test]
    fn render_errors_on_unknown_edge_endpoint() {
        let graph = WorkflowGraph {
            nodes: vec![node("a")],
            edges: vec![edge("a", "ghost", WorkflowEdgeKind::Spawn)],
        };
        assert!(render(&graph).is_err());
    }
}
