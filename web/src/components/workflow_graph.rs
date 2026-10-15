//! Render a catalog [`WorkflowGraph`] as an inline SVG DAG.
//!
//! Nodes are catalog tasks (the name, plus `scheduled`/`external` styling);
//! edges are directed and labelled by [`WorkflowEdgeKind`]. Layout is a pure,
//! cycle-safe layered assignment, so the component needs no Graphviz or DAG
//! dependency — mirroring the TUI's `ascii-dag` semantics for back edges and
//! self-loops.
//!
//! The panel is presentational: selecting a node calls back into the view (which
//! owns the router) so the component mounts without a `Router` context in host
//! tests.

use std::collections::{HashMap, VecDeque};

use dioxus::prelude::*;
use favetto_wire::workflow::{WorkflowEdge, WorkflowEdgeKind, WorkflowGraph, WorkflowNode};

use crate::state::task_display_name;

/// Node box width in SVG user units.
pub const NODE_W: f32 = 156.0;
/// Node box height in SVG user units.
pub const NODE_H: f32 = 40.0;
/// Horizontal gap between layers.
const H_GAP: f32 = 64.0;
/// Vertical gap between rows within a layer.
const V_GAP: f32 = 22.0;
/// Outer canvas margin.
const MARGIN: f32 = 16.0;

/// Human label for a catalog graph edge kind.
pub fn edge_kind_label(kind: WorkflowEdgeKind) -> &'static str {
    match kind {
        WorkflowEdgeKind::Spawn => "spawn",
        WorkflowEdgeKind::Needs => "needs",
        WorkflowEdgeKind::NeedsSucceeded => "needs:succeeded",
        WorkflowEdgeKind::NeedsFailed => "needs:failed",
        WorkflowEdgeKind::Join => "join",
    }
}

/// Class-safe slug for an edge kind (used as `workflow-edge-<slug>`).
pub fn edge_kind_slug(kind: WorkflowEdgeKind) -> &'static str {
    match kind {
        WorkflowEdgeKind::Spawn => "spawn",
        WorkflowEdgeKind::Needs => "needs",
        WorkflowEdgeKind::NeedsSucceeded => "needs-succeeded",
        WorkflowEdgeKind::NeedsFailed => "needs-failed",
        WorkflowEdgeKind::Join => "join",
    }
}

/// Split `graph` into its weakly-connected components (edges treated as
/// undirected). Isolated tasks become single-node components.
///
/// Components are ordered deterministically: by node count descending, then by
/// the lexicographically smallest node name, then by the full name list. Node
/// and edge order within a component follows `graph`.
pub fn weakly_connected_components(graph: &WorkflowGraph) -> Vec<WorkflowGraph> {
    let n = graph.nodes.len();
    if n == 0 {
        return Vec::new();
    }

    let index: HashMap<&str, usize> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.name.as_str(), i))
        .collect();

    let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); n];
    for edge in &graph.edges {
        let (Some(&from), Some(&to)) = (index.get(edge.from.as_str()), index.get(edge.to.as_str()))
        else {
            // Defensive: `build_graph` never emits an edge to a missing node.
            continue;
        };
        if from == to {
            continue;
        }
        adjacency[from].push(to);
        adjacency[to].push(from);
    }

    let mut visited = vec![false; n];
    let mut components: Vec<WorkflowGraph> = Vec::new();
    for start in 0..n {
        if visited[start] {
            continue;
        }
        let mut members = vec![false; n];
        let mut queue = VecDeque::new();
        queue.push_back(start);
        visited[start] = true;
        while let Some(node) = queue.pop_front() {
            members[node] = true;
            for &neighbor in &adjacency[node] {
                if !visited[neighbor] {
                    visited[neighbor] = true;
                    queue.push_back(neighbor);
                }
            }
        }

        let nodes: Vec<WorkflowNode> = graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(i, _)| members[*i])
            .map(|(_, node)| node.clone())
            .collect();
        let edges: Vec<WorkflowEdge> = graph
            .edges
            .iter()
            .filter(|edge| {
                index.get(edge.from.as_str()).is_some_and(|&i| members[i])
                    && index.get(edge.to.as_str()).is_some_and(|&i| members[i])
            })
            .cloned()
            .collect();
        components.push(WorkflowGraph { nodes, edges });
    }

    components.sort_by(|a, b| {
        b.nodes
            .len()
            .cmp(&a.nodes.len())
            .then_with(|| {
                a.nodes
                    .iter()
                    .map(|node| node.name.as_str())
                    .min()
                    .cmp(&b.nodes.iter().map(|node| node.name.as_str()).min())
            })
            .then_with(|| {
                let a_names: Vec<&str> = a.nodes.iter().map(|node| node.name.as_str()).collect();
                let b_names: Vec<&str> = b.nodes.iter().map(|node| node.name.as_str()).collect();
                a_names.cmp(&b_names)
            })
    });

    components
}

/// Short label for a component in the workflow picker.
pub fn component_label(graph: &WorkflowGraph) -> String {
    match graph.nodes.first() {
        None => "Empty workflow".to_string(),
        Some(first) if graph.nodes.len() == 1 => first.name.clone(),
        Some(first) => format!("{} (+{} more)", first.name, graph.nodes.len() - 1),
    }
}

/// A placed graph node.
#[derive(Debug, Clone, PartialEq)]
pub struct NodePos {
    pub name: String,
    pub x: f32,
    pub y: f32,
    pub scheduled: bool,
    pub external: bool,
}

/// A laid-out graph: node positions plus the canvas size.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphLayout {
    pub nodes: Vec<NodePos>,
    pub width: f32,
    pub height: f32,
}

/// Tarjan's strongly-connected components over `adjacency` (indexed by node).
///
/// Returns each node's component id plus the component count. Contracting each
/// component yields a DAG (the condensation), which is what [`layout`] layers.
fn strongly_connected_components(n: usize, adjacency: &[Vec<usize>]) -> (Vec<usize>, usize) {
    struct Tarjan {
        index: usize,
        indices: Vec<Option<usize>>,
        low: Vec<usize>,
        on_stack: Vec<bool>,
        stack: Vec<usize>,
        component: Vec<usize>,
        count: usize,
    }

    fn visit(v: usize, adjacency: &[Vec<usize>], t: &mut Tarjan) {
        t.indices[v] = Some(t.index);
        t.low[v] = t.index;
        t.index += 1;
        t.stack.push(v);
        t.on_stack[v] = true;

        for &w in &adjacency[v] {
            if t.indices[w].is_none() {
                visit(w, adjacency, t);
                t.low[v] = t.low[v].min(t.low[w]);
            } else if t.on_stack[w] {
                // `w` is on the current DFS stack, so it has an index.
                let w_index = t.indices[w].unwrap_or_default();
                t.low[v] = t.low[v].min(w_index);
            }
        }

        if t.low[v] == t.indices[v].unwrap_or_default() {
            loop {
                let w = t.stack.pop().expect("the component root is on the stack");
                t.on_stack[w] = false;
                t.component[w] = t.count;
                if w == v {
                    break;
                }
            }
            t.count += 1;
        }
    }

    let mut t = Tarjan {
        index: 0,
        indices: vec![None; n],
        low: vec![0; n],
        on_stack: vec![false; n],
        stack: Vec::new(),
        component: vec![0; n],
        count: 0,
    };
    for v in 0..n {
        if t.indices[v].is_none() {
            visit(v, adjacency, &mut t);
        }
    }
    (t.component, t.count)
}

/// Assign every node a grid position. Deterministic and cycle-safe: each
/// strongly-connected component shares a single column, and the components are
/// then layered by longest path over the (acyclic) condensation graph. Nodes
/// within a column keep `graph.nodes` order.
pub fn layout(graph: &WorkflowGraph) -> GraphLayout {
    let n = graph.nodes.len();
    if n == 0 {
        return GraphLayout {
            nodes: Vec::new(),
            width: 2.0 * MARGIN + NODE_W,
            height: 2.0 * MARGIN + NODE_H,
        };
    }

    let index: HashMap<&str, usize> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.name.as_str(), i))
        .collect();

    let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); n];
    for edge in &graph.edges {
        let (Some(&from), Some(&to)) = (index.get(edge.from.as_str()), index.get(edge.to.as_str()))
        else {
            continue;
        };
        if from != to {
            adjacency[from].push(to);
        }
    }

    // Collapse cycles first: every strongly-connected component gets one column.
    // Layering the *condensation* keeps a cyclic graph as compact as an acyclic
    // one — otherwise a cycle inflates the longest path to O(nodes) columns, so
    // the canvas grows enormous and the graph renders tiny and lopsided.
    let (component, component_count) = strongly_connected_components(n, &adjacency);
    let mut condensation: Vec<Vec<usize>> = vec![Vec::new(); component_count];
    for (from, targets) in adjacency.iter().enumerate() {
        for &to in targets {
            if component[from] != component[to] {
                condensation[component[from]].push(component[to]);
            }
        }
    }

    let mut component_layer = vec![0usize; component_count];
    for _ in 0..component_count {
        let mut changed = false;
        for (from, targets) in condensation.iter().enumerate() {
            for &to in targets {
                if component_layer[to] < component_layer[from] + 1 {
                    component_layer[to] = component_layer[from] + 1;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    let layer: Vec<usize> = (0..n).map(|i| component_layer[component[i]]).collect();
    let layer_count = layer.iter().copied().max().map_or(1, |max| max + 1);
    let mut rows = vec![0usize; layer_count];
    let mut nodes = Vec::with_capacity(n);
    for (i, node) in graph.nodes.iter().enumerate() {
        let lane = layer[i];
        let row = rows[lane];
        rows[lane] += 1;
        nodes.push(NodePos {
            name: node.name.clone(),
            x: MARGIN + lane as f32 * (NODE_W + H_GAP),
            y: MARGIN + row as f32 * (NODE_H + V_GAP),
            scheduled: node.scheduled,
            external: node.external,
        });
    }

    let mut width = 2.0 * MARGIN + NODE_W;
    let mut height = 2.0 * MARGIN + NODE_H;
    for node in &nodes {
        width = width.max(node.x + NODE_W + MARGIN);
        height = height.max(node.y + NODE_H + MARGIN);
    }

    GraphLayout {
        nodes,
        width,
        height,
    }
}

/// One SVG `<g>` node. Non-external nodes are clickable (via `on_select`).
fn node_element(node: &NodePos, on_select: EventHandler<String>) -> Element {
    let mut class = String::from("workflow-graph-node");
    if node.external {
        class.push_str(" external");
    }
    if node.scheduled {
        class.push_str(" scheduled");
    }
    let interactive = !node.external;
    let x = node.x;
    let y = node.y;
    let center_x = x + NODE_W / 2.0;
    let center_y = y + NODE_H / 2.0;
    // Only the rendered text is stripped: `node.name` stays the qualified
    // identity used to route the click/key selection to `CatalogDetail`.
    let label = task_display_name(&node.name).to_string();
    let click_label = node.name.clone();
    let key_label = node.name.clone();

    rsx! {
        g {
            class: "{class}",
            role: if interactive { "link" } else { "img" },
            tabindex: if interactive { 0 } else { -1 },
            onclick: move |_| {
                if interactive {
                    on_select.call(click_label.clone());
                }
            },
            onkeydown: move |event| {
                if interactive && event.key() == Key::Enter {
                    on_select.call(key_label.clone());
                }
            },
            rect { x: "{x}", y: "{y}", width: "{NODE_W}", height: "{NODE_H}", rx: "6" }
            text {
                x: "{center_x}",
                y: "{center_y}",
                text_anchor: "middle",
                dominant_baseline: "middle",
                "{label}"
            }
        }
    }
}

/// One SVG edge: a `<path>` plus its kind label.
fn edge_element(edge: &WorkflowEdge, x1: f32, y1: f32, x2: f32, y2: f32) -> Element {
    let half = NODE_H / 2.0;
    let self_edge = edge.from == edge.to;
    let back = x2 <= x1;

    let (d, label_x, label_y) = if self_edge {
        let start_x = x1 + NODE_W;
        let start_y = y1 + half;
        (
            format!(
                "M {start_x} {start_y} C {} {}, {} {}, {start_x} {start_y}",
                start_x + 52.0,
                y1 - 42.0,
                start_x + 52.0,
                y1 + NODE_H + 42.0
            ),
            start_x + 62.0,
            y1 + half,
        )
    } else if back {
        let from_x = x1 + NODE_W / 2.0;
        let to_x = x2 + NODE_W / 2.0;
        (
            format!(
                "M {from_x} {} C {from_x} {}, {to_x} {}, {to_x} {}",
                y1 + NODE_H,
                y1 + NODE_H + 40.0,
                y2 - 40.0,
                y2
            ),
            (from_x + to_x) / 2.0,
            (y1 + NODE_H + y2) / 2.0,
        )
    } else {
        let from_y = y1 + half;
        let to_y = y2 + half;
        (
            format!(
                "M {} {from_y} C {} {from_y}, {} {to_y}, {x2} {to_y}",
                x1 + NODE_W,
                x1 + NODE_W + 40.0,
                x2 - 40.0
            ),
            (x1 + NODE_W + x2) / 2.0,
            (from_y + to_y) / 2.0,
        )
    };

    let mut class = format!("workflow-edge workflow-edge-{}", edge_kind_slug(edge.kind));
    if self_edge || back {
        class.push_str(" workflow-edge-back");
    }
    let label = edge_kind_label(edge.kind);

    rsx! {
        path { class: "{class}", d: "{d}", marker_end: "url(#workflow-arrow)" }
        text {
            class: "workflow-edge-label",
            x: "{label_x}",
            y: "{label_y}",
            text_anchor: "middle",
            "{label}"
        }
    }
}

/// Render `graph` as an SVG DAG. `on_select` fires with a node's catalog name
/// when a non-external node is activated (click or Enter).
#[component]
pub fn WorkflowGraphView(graph: WorkflowGraph, on_select: EventHandler<String>) -> Element {
    let layout = layout(&graph);
    let width = layout.width;
    let height = layout.height;
    let positions: HashMap<&str, (f32, f32)> = layout
        .nodes
        .iter()
        .map(|node| (node.name.as_str(), (node.x, node.y)))
        .collect();

    rsx! {
        div { class: "workflow-graph-wrap",
            svg {
                class: "workflow-graph",
                // Fill the panel, but never grow past the graph's natural size so
                // small graphs are not over-zoomed. `preserveAspectRatio` centers
                // the drawing in whatever box results, so the DAG stays visible.
                style: "width: 100%; height: 100%; max-width: {width}px; max-height: {height}px",
                view_box: "0 0 {width} {height}",
                preserve_aspect_ratio: "xMidYMid meet",
                role: "img",
                "aria-label": "Catalog task dependency graph",
                defs {
                    marker {
                        id: "workflow-arrow",
                        view_box: "0 0 10 10",
                        ref_x: "9",
                        ref_y: "5",
                        marker_width: "7",
                        marker_height: "7",
                        orient: "auto-start-reverse",
                        path { d: "M 0 0 L 10 5 L 0 10 z", fill: "context-stroke" }
                    }
                }
                for edge in graph.edges.iter() {
                    if let (Some(&(x1, y1)), Some(&(x2, y2))) = (
                        positions.get(edge.from.as_str()),
                        positions.get(edge.to.as_str()),
                    ) {
                        { edge_element(edge, x1, y1, x2, y2) }
                    }
                }
                for node in layout.nodes.iter() {
                    { node_element(node, on_select) }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn graph(nodes: &[&str], edges: &[WorkflowEdge]) -> WorkflowGraph {
        WorkflowGraph {
            nodes: nodes.iter().map(|name| node(name)).collect(),
            edges: edges.to_vec(),
        }
    }

    fn names(component: &WorkflowGraph) -> Vec<String> {
        component
            .nodes
            .iter()
            .map(|node| node.name.clone())
            .collect()
    }

    #[test]
    fn edge_kind_labels_are_distinct() {
        let labels = [
            edge_kind_label(WorkflowEdgeKind::Spawn),
            edge_kind_label(WorkflowEdgeKind::Needs),
            edge_kind_label(WorkflowEdgeKind::NeedsSucceeded),
            edge_kind_label(WorkflowEdgeKind::NeedsFailed),
            edge_kind_label(WorkflowEdgeKind::Join),
        ];
        let unique: std::collections::HashSet<&str> = labels.iter().copied().collect();
        assert_eq!(unique.len(), labels.len());
        // Slugs are class-safe (no `:`) and also distinct.
        let slugs = [
            edge_kind_slug(WorkflowEdgeKind::Spawn),
            edge_kind_slug(WorkflowEdgeKind::Needs),
            edge_kind_slug(WorkflowEdgeKind::NeedsSucceeded),
            edge_kind_slug(WorkflowEdgeKind::NeedsFailed),
            edge_kind_slug(WorkflowEdgeKind::Join),
        ];
        for slug in slugs {
            assert!(!slug.contains(':'), "{slug}");
        }
    }

    #[test]
    fn components_isolate_unconnected_nodes() {
        let graph = graph(&["a", "b", "c"], &[]);
        let components = weakly_connected_components(&graph);
        assert_eq!(components.len(), 3);
        for component in &components {
            assert_eq!(component.nodes.len(), 1);
        }
        // Ties break on the smallest name.
        assert_eq!(names(&components[0]), vec!["a"]);
        assert_eq!(names(&components[1]), vec!["b"]);
        assert_eq!(names(&components[2]), vec!["c"]);
    }

    #[test]
    fn components_join_a_connected_pair() {
        let graph = graph(&["a", "b"], &[edge("a", "b", WorkflowEdgeKind::Spawn)]);
        let components = weakly_connected_components(&graph);
        assert_eq!(components.len(), 1);
        assert_eq!(names(&components[0]), vec!["a", "b"]);
        assert_eq!(components[0].edges.len(), 1);
    }

    #[test]
    fn components_order_largest_first_then_by_name() {
        let graph = graph(
            &["a", "b", "c", "d"],
            &[
                edge("a", "b", WorkflowEdgeKind::Spawn),
                edge("b", "c", WorkflowEdgeKind::Needs),
            ],
        );
        let components = weakly_connected_components(&graph);
        assert_eq!(components.len(), 2);
        assert_eq!(names(&components[0]), vec!["a", "b", "c"]);
        assert_eq!(names(&components[1]), vec!["d"]);
    }

    #[test]
    fn components_treat_edges_as_undirected_and_terminate_on_a_cycle() {
        let graph = graph(
            &["a", "b", "c"],
            &[
                edge("a", "b", WorkflowEdgeKind::Spawn),
                edge("b", "a", WorkflowEdgeKind::Needs),
                edge("c", "c", WorkflowEdgeKind::Needs),
            ],
        );
        let components = weakly_connected_components(&graph);
        assert_eq!(components.len(), 2);
        assert_eq!(names(&components[0]), vec!["a", "b"]);
        // The self-loop stays inside its singleton component.
        assert_eq!(names(&components[1]), vec!["c"]);
        assert_eq!(components[1].edges.len(), 1);
    }

    #[test]
    fn components_ignore_edges_to_unknown_nodes() {
        let graph = graph(&["a"], &[edge("a", "ghost", WorkflowEdgeKind::Spawn)]);
        let components = weakly_connected_components(&graph);
        assert_eq!(components.len(), 1);
        assert_eq!(names(&components[0]), vec!["a"]);
        assert!(components[0].edges.is_empty());
    }

    #[test]
    fn component_labels_name_singletons_and_count_groups() {
        assert_eq!(component_label(&graph(&["solo"], &[])), "solo");
        assert_eq!(
            component_label(&graph(&["a", "b", "c"], &[])),
            "a (+2 more)"
        );
        assert_eq!(component_label(&graph(&[], &[])), "Empty workflow");
    }

    #[test]
    fn layout_places_targets_to_the_right_of_sources() {
        let graph = graph(&["a", "b"], &[edge("a", "b", WorkflowEdgeKind::Spawn)]);
        let layout = layout(&graph);
        let a = layout.nodes.iter().find(|node| node.name == "a").unwrap();
        let b = layout.nodes.iter().find(|node| node.name == "b").unwrap();
        assert!(b.x > a.x, "b={} a={}", b.x, a.x);
        assert!(a.x.is_finite() && a.y.is_finite());
        assert!(b.x.is_finite() && b.y.is_finite());
    }

    #[test]
    fn layout_is_deterministic_and_cycle_safe() {
        let graph = graph(
            &["a", "b"],
            &[
                edge("a", "b", WorkflowEdgeKind::Spawn),
                edge("b", "a", WorkflowEdgeKind::Needs),
            ],
        );
        let first = layout(&graph);
        let second = layout(&graph);
        assert_eq!(first, second);
        for node in &first.nodes {
            assert!(node.x.is_finite() && node.y.is_finite());
        }
    }

    #[test]
    fn layout_has_a_non_degenerate_canvas() {
        let empty = layout(&graph(&[], &[]));
        assert!(empty.width >= 2.0 * MARGIN + NODE_W);
        assert!(empty.height >= 2.0 * MARGIN + NODE_H);

        let single = layout(&graph(&["only"], &[]));
        assert!(single.width >= 2.0 * MARGIN + NODE_W);
        assert!(single.height >= 2.0 * MARGIN + NODE_H);
    }

    #[test]
    fn layout_terminates_on_a_self_loop() {
        let graph = graph(&["a"], &[edge("a", "a", WorkflowEdgeKind::Needs)]);
        let layout = layout(&graph);
        assert_eq!(layout.nodes.len(), 1);
        assert!(layout.nodes[0].x.is_finite());
    }

    #[test]
    fn layout_keeps_a_four_node_cycle_compact() {
        // The `implement_issue` shape: triage -> plan -> implement -> merge ->
        // triage. The cycle-closing edge must not inflate the layers, or the
        // canvas grows huge and the graph renders tiny and lopsided.
        let graph = graph(
            &["triage", "plan", "implement", "merge"],
            &[
                edge("triage", "plan", WorkflowEdgeKind::Spawn),
                edge("plan", "implement", WorkflowEdgeKind::Spawn),
                edge("implement", "merge", WorkflowEdgeKind::Needs),
                edge("merge", "triage", WorkflowEdgeKind::Spawn),
            ],
        );
        let layout = layout(&graph);
        // One SCC -> a single column.
        let columns: std::collections::HashSet<i32> =
            layout.nodes.iter().map(|node| node.x as i32).collect();
        assert_eq!(columns.len(), 1, "the cycle should occupy one column");
        assert!(
            layout.width <= 2.0 * MARGIN + NODE_W + 1.0,
            "width {} should stay near a single column",
            layout.width
        );
        assert_eq!(layout.nodes.len(), 4);
    }

    #[test]
    fn layout_places_a_cycle_after_its_dependencies() {
        // entry -> {a <-> b}: the entry sits one column left of the cycle, and
        // both cycle members share that column.
        let graph = graph(
            &["entry", "a", "b"],
            &[
                edge("entry", "a", WorkflowEdgeKind::Spawn),
                edge("a", "b", WorkflowEdgeKind::Needs),
                edge("b", "a", WorkflowEdgeKind::Needs),
            ],
        );
        let layout = layout(&graph);
        let x = |name: &str| layout.nodes.iter().find(|n| n.name == name).unwrap().x;
        assert!(x("entry") < x("a"), "entry={} a={}", x("entry"), x("a"));
        assert_eq!(x("a"), x("b"));
    }

    #[test]
    fn layout_layers_a_chain_by_longest_path() {
        let graph = graph(
            &["a", "b", "c"],
            &[
                edge("a", "b", WorkflowEdgeKind::Spawn),
                edge("b", "c", WorkflowEdgeKind::Needs),
            ],
        );
        let layout = layout(&graph);
        let x = |name: &str| layout.nodes.iter().find(|n| n.name == name).unwrap().x;
        assert!(x("a") < x("b") && x("b") < x("c"));
        assert_eq!(x("b") - x("a"), x("c") - x("b"));
    }

    fn spawn_graph() -> WorkflowGraph {
        graph(&["a", "b"], &[edge("a", "b", WorkflowEdgeKind::Spawn)])
    }

    fn cyclic_graph() -> WorkflowGraph {
        graph(
            &["a", "b"],
            &[
                edge("a", "b", WorkflowEdgeKind::Spawn),
                edge("b", "a", WorkflowEdgeKind::Needs),
                edge("a", "a", WorkflowEdgeKind::Needs),
            ],
        )
    }

    #[test]
    fn workflow_graph_view_mounts_host_side() {
        let mut dom = VirtualDom::new(|| {
            rsx! {
                WorkflowGraphView { graph: spawn_graph(), on_select: move |_| {} }
            }
        });
        dom.rebuild_in_place();
    }

    #[test]
    fn workflow_graph_view_mounts_with_a_cycle() {
        let mut dom = VirtualDom::new(|| {
            rsx! {
                WorkflowGraphView { graph: cyclic_graph(), on_select: move |_| {} }
            }
        });
        dom.rebuild_in_place();
    }
}
