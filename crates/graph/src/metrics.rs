//! Graph metrics and DOT export (task 1.5).
//!
//! Oracle mapping: `../synopsis/internal/graph/metrics.go` (`Stats`,
//! `ToDOT`) — a functional copy, re-architected for Rust (migration
//! principle: not a code copy).
//!
//! ## Stats
//!
//! [`Graph::stats`] returns [`GraphStats`]: node count, edge count and the
//! average degree.
//!
//! ### Edge count: both populations
//!
//! `edge_count` covers fact edges AND entity-link edges — consistent with
//! [`Graph::edge_count`] (task 1.2) and the oracle's `NewGraphFromDB`
//! stats. The oracle's `Stats()` method, in contrast, reports fact edges
//! only — an internal inconsistency in the oracle (its own `TestGraphStats`
//! uses facts only, so it never catches it). Rust reports the superset.
//!
//! ### avg_degree = 2E/N (directed-graph formula)
//!
//! The average TOTAL degree (in + out) per node: every directed edge
//! contributes exactly one out-degree and one in-degree, so the sum of all
//! node degrees is `2E` and the average is `2E / N` (0.0 for an empty
//! index — the oracle's zero-node guard).
//!
//! The oracle's `avgDegree` sums its outgoing and incoming adjacency maps
//! (plus the cross-domain maps). Every edge is stored once in an outgoing
//! map and once in an incoming map, so that sum is `2E` — the oracle's own
//! tests pin exactly the `2E/N` values (2 nodes / 2 edges → 2.0 in
//! `TestGraphStats` and `TestGraphStats_AvgDegreeIncludesEntityLinks`). An
//! earlier note (task 1.2, graph.rs) read the outgoing + incoming summation
//! as a double-counting bug producing `4E/N`; the oracle's source and tests
//! do not bear that out. The Rust implementation derives the same `2E/N`
//! directly from the `DiGraph` edge count, with no per-map bookkeeping.
//!
//! ## DOT export (the oracle's `ToDOT`)
//!
//! [`Graph::to_dot`] renders the FULL index as Graphviz DOT through
//! petgraph's `Dot` writer (design D1: DOT out of the box). Node labels are
//! entity names and edge labels are relation types; petgraph's writer
//! escapes both (`"` → `\"`, `\` → `\\`, newlines → `\l`).
//!
//! Deviations from the oracle:
//! - The oracle exports a BFS-reachable SUBGRAPH from a start node under
//!   `BFSOptions` (error on an unknown start node). No consumer of the
//!   scoped variant exists — the full-index export is infallible and covers
//!   the diagnostic purpose; a scoped variant is addable later without
//!   breaking the contract.
//! - Node attribution: the oracle put the entity type into a hardcoded
//!   fillcolor palette (`typeColor`, oracle-specific lowercase types); here
//!   domain and type are plain node ATTRIBUTES, plus the entity row id as
//!   `entity_id` so the export maps back to the `entities` table.
//! - Entity-link edges are drawn `style = dashed` to distinguish the two
//!   edge populations (the oracle drew both solid).
//! - The oracle names the digraph `KnowledgeBase`; petgraph writes an
//!   unnamed `digraph {` (valid DOT; the writer has no name hook).
//! - Custom attribute strings are written verbatim by petgraph, so their
//!   VALUES are escaped by [`escape_attr`] (same rules as petgraph's label
//!   escaper).
//!
//! ## Not ported (YAGNI)
//!
//! - `MaxConnectedDepth`: defined in the oracle but never called (no
//!   caller, no test, no MCP tool) and expensive (a BFS from every node).
//!   Deferred; addable later without breaking the contract.
//! - `GraphStats.LoadDuration`: the oracle sets it only in
//!   `NewGraphFromDB` (the `Stats()` method leaves it zero). Callers that
//!   need a load timing measure it around `GraphIndex::from_db`.

use std::fmt;

use petgraph::dot::{Config, Dot, RankDir};
use petgraph::graph::{DiGraph, EdgeReference, NodeIndex};

use crate::graph::{EdgeKind, EntityNode, Graph, GraphEdge};

/// DOT layout direction: left to right (the oracle's `rankdir=LR`).
const DOT_CONFIG: [Config; 1] = [Config::RankDir(RankDir::LR)];

/// Summary statistics of a built index (the oracle's `GraphStats`, minus the
/// build-time-only `load_duration` — see the module docs).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GraphStats {
    /// Number of nodes (entities).
    pub node_count: usize,
    /// Number of edges: fact edges + entity-link edges (both populations;
    /// consistent with [`Graph::edge_count`]).
    pub edge_count: usize,
    /// Average total degree (in + out) per node: `2 * edge_count /
    /// node_count`; 0.0 for an empty index.
    pub avg_degree: f64,
}

impl Graph {
    /// Summary statistics of the index (the oracle's `Stats`).
    pub fn stats(&self) -> GraphStats {
        GraphStats {
            node_count: self.node_count(),
            edge_count: self.edge_count(),
            avg_degree: self.avg_degree(),
        }
    }

    /// Average total degree (in + out) per node (the oracle's `avgDegree`):
    /// `2 * E / N` — every directed edge contributes exactly one out-degree
    /// and one in-degree, so the sum of all node degrees is `2E`.
    ///
    /// An empty index yields 0.0 (the oracle's zero-node guard).
    pub fn avg_degree(&self) -> f64 {
        let nodes = self.node_count();
        if nodes == 0 {
            return 0.0;
        }
        2.0 * self.edge_count() as f64 / nodes as f64
    }

    /// Export the FULL index as Graphviz DOT (the oracle's `ToDOT` without
    /// the BFS subgraph scope — see the module docs).
    ///
    /// Infallible: an empty index renders an empty, valid digraph. Node
    /// labels are entity names and edge labels are relation types (petgraph
    /// escapes both); nodes carry `entity_id`, `domain` and `type`
    /// attributes; entity-link edges are drawn dashed.
    pub fn to_dot(&self) -> String {
        let dot = Dot::with_attr_getters(
            self.graph(),
            &DOT_CONFIG,
            &edge_attributes,
            &node_attributes,
        );
        DotExport { dot }.to_string()
    }
}

/// Node attribute string for the DOT export: the entity row id (maps the
/// node back to the `entities` table), the normalized domain and the entity
/// type. petgraph writes custom attribute strings verbatim, so the values
/// are escaped by [`escape_attr`].
fn node_attributes(
    _g: &DiGraph<EntityNode, GraphEdge>,
    (_, node): (NodeIndex, &EntityNode),
) -> String {
    format!(
        r#"entity_id = {}, domain = "{}", type = "{}", shape = ellipse"#,
        node.id,
        escape_attr(&node.domain),
        escape_attr(&node.entity_type),
    )
}

/// Edge attribute string for the DOT export: entity-link edges are drawn
/// dashed to distinguish the two edge populations (the D4 boundary key);
/// fact edges keep the default solid style.
fn edge_attributes(
    _g: &DiGraph<EntityNode, GraphEdge>,
    edge: EdgeReference<'_, GraphEdge>,
) -> String {
    match edge.weight().kind {
        EdgeKind::Fact => String::new(),
        EdgeKind::EntityLink => "style = dashed".to_string(),
    }
}

/// Escape a value for a DOT attribute string generated by this module.
///
/// petgraph escapes LABELS written through its `graph_fmt` label slot, but
/// custom attribute strings are written verbatim — so this helper mirrors
/// petgraph's own escaper rules: `"` → `\"`, `\` → `\\`, `\n` → `\l`.
fn escape_attr(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\l"),
            _ => out.push(c),
        }
    }
    out
}

/// Renders a configured [`Dot`] into a `String`.
///
/// petgraph's `Dot` implements `Display` only for graphs whose weights
/// implement `Display`; the index weights are plain data types, so the
/// label logic (entity name / relation type) is routed through
/// [`Dot::graph_fmt`] instead — keeping `Display` off the public weight
/// types.
struct DotExport<'g, 'cfg> {
    dot: Dot<'cfg, &'g DiGraph<EntityNode, GraphEdge>>,
}

impl fmt::Display for DotExport<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.dot.graph_fmt(
            f,
            |node: &EntityNode, f| f.write_str(&node.name),
            |edge: &GraphEdge, f| f.write_str(&edge.relation_type),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use db::{Entity, EntityLink, Fact};

    // ── fixtures (same shape as the traverser tests) ─────────────────────

    fn entity(id: i64, entity_type: &str, name: &str, domain: &str) -> Entity {
        Entity {
            id,
            entity_type: entity_type.into(),
            name: name.into(),
            domain: domain.into(),
            description: None,
            confidence: None,
            metadata_json: None,
            created_at: String::new(),
        }
    }

    fn fact(id: i64, subject: i64, predicate: &str, object: i64) -> Fact {
        Fact {
            id,
            subject_entity_id: Some(subject),
            predicate: predicate.into(),
            object_entity_id: Some(object),
            domain: String::new(),
            metadata_json: None,
            status: "approved".into(),
            valid_from: None,
            valid_to: None,
            weight: 0,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn link(
        subject: i64,
        target: i64,
        relation_type: &str,
        method: &str,
        confidence: f64,
        evidence: Option<&str>,
    ) -> EntityLink {
        EntityLink {
            subject_entity_id: subject,
            target_entity_id: target,
            relation_type: relation_type.into(),
            method: method.into(),
            confidence,
            evidence: evidence.map(str::to_owned),
        }
    }

    // Acceptance: the counters match the built index — the oracle's
    // TestGraphStats shape (2 nodes, 2 fact edges).
    #[test]
    fn stats_match_built_index() {
        let graph = Graph::from_rows(
            vec![
                entity(1, "employee", "NodeA", ""),
                entity(2, "department", "NodeB", ""),
            ],
            vec![fact(1, 1, "located_in", 2), fact(2, 2, "connected_to", 1)],
            Vec::new(),
        );
        let stats = graph.stats();
        assert_eq!(stats.node_count, 2);
        assert_eq!(stats.edge_count, 2);
        assert_eq!(
            stats.avg_degree, 2.0,
            "2E/N = 4/2 (the oracle's TestGraphStats)"
        );
    }

    // Acceptance: entity-link edges count toward the edge count and the
    // average degree (the oracle's m-11 /
    // TestGraphStats_AvgDegreeIncludesEntityLinks).
    #[test]
    fn stats_include_entity_links() {
        let graph = Graph::from_rows(
            vec![
                entity(1, "employee", "NodeA", "hr"),
                entity(2, "employee", "NodeB", "policy"),
            ],
            Vec::new(),
            vec![
                link(1, 2, "same_as", "rule", 1.0, None),
                link(2, 1, "same_as", "rule", 1.0, None),
            ],
        );
        let stats = graph.stats();
        assert_eq!(stats.node_count, 2);
        assert_eq!(stats.edge_count, 2, "link edges are edges");
        assert_eq!(stats.avg_degree, 2.0, "2E/N = 4/2");
    }

    // The directed-graph formula on a non-symmetric shape: 3 nodes, 2 edges
    // → 2E/N = 4/3.
    #[test]
    fn avg_degree_is_2e_over_n() {
        let graph = Graph::from_rows(
            vec![
                entity(1, "PERSON", "Alice", "hr"),
                entity(2, "PERSON", "Bob", "hr"),
                entity(3, "ORGANIZATION", "Acme", "it"),
            ],
            vec![fact(1, 1, "knows", 2), fact(2, 1, "works_at", 3)],
            Vec::new(),
        );
        let stats = graph.stats();
        assert_eq!(stats.node_count, 3);
        assert_eq!(stats.edge_count, 2);
        assert!(
            (stats.avg_degree - 4.0 / 3.0).abs() < f64::EPSILON,
            "2*2/3, got {}",
            stats.avg_degree
        );
    }

    // Degenerate: an empty index → zeros, no NaN (the oracle's zero-node
    // guard).
    #[test]
    fn empty_index_stats_are_zero() {
        let stats = Graph::empty().stats();
        assert_eq!(stats.node_count, 0);
        assert_eq!(stats.edge_count, 0);
        assert_eq!(stats.avg_degree, 0.0);
        assert!(stats.avg_degree.is_finite());
    }

    // Acceptance: DOT is non-empty and structurally valid — the digraph
    // header, the closing brace, node names, the domain/type attribution,
    // the relation labels.
    #[test]
    fn dot_is_structurally_valid() {
        let graph = Graph::from_rows(
            vec![
                entity(1, "PERSON", "Alice", "hr"),
                entity(2, "ORGANIZATION", "Acme", "it"),
            ],
            vec![fact(1, 1, "works_at", 2)],
            vec![link(2, 1, "related_to", "equals", 0.8, None)],
        );
        let dot = graph.to_dot();
        assert!(!dot.is_empty(), "DOT must be non-empty");
        assert!(dot.starts_with("digraph {"), "digraph header: {dot}");
        assert!(dot.ends_with("}\n"), "closing brace: {dot}");
        assert!(dot.contains(r#"rankdir="LR""#));
        // Node attribution: name label + domain/type/entity_id attributes.
        assert!(dot.contains(r#"label = "Alice""#));
        assert!(dot.contains(r#"label = "Acme""#));
        assert!(dot.contains(r#"domain = "hr""#));
        assert!(dot.contains(r#"domain = "it""#));
        assert!(dot.contains(r#"type = "PERSON""#));
        assert!(dot.contains(r#"type = "ORGANIZATION""#));
        assert!(dot.contains("entity_id = 1"));
        // Edges: relation labels; entity links drawn dashed.
        assert!(dot.contains("->"));
        assert!(dot.contains(r#"label = "works_at""#));
        assert!(dot.contains(r#"label = "related_to""#));
        assert!(dot.contains("style = dashed"));
    }

    // Acceptance: escaping of special characters in names — petgraph's
    // escaper rules (`"` → `\"`, `\` → `\\`, `\n` → `\l`).
    #[test]
    fn dot_escapes_special_characters_in_names() {
        let graph = Graph::from_rows(
            vec![
                entity(1, "PERSON", "Alice \"A\" \\ Bob\nCarol", "hr"),
                entity(2, "PERSON", "Bob", "hr"),
            ],
            vec![fact(1, 1, "says \"hi\"", 2)],
            Vec::new(),
        );
        let dot = graph.to_dot();
        // The name label and the relation label are escaped by petgraph…
        assert!(
            dot.contains(r#"label = "Alice \"A\" \\ Bob\lCarol""#),
            "{dot}"
        );
        assert!(dot.contains(r#"label = "says \"hi\"""#), "{dot}");
        // …so the raw (unescaped) sequences must not appear anywhere.
        assert!(!dot.contains("\"A\""), "raw quotes in output: {dot}");
        assert!(!dot.contains("\"hi\""), "raw quotes in output: {dot}");
    }

    // An empty index renders a valid (empty) digraph: non-empty output with
    // the header and the closing brace.
    #[test]
    fn dot_of_empty_index_is_a_valid_empty_digraph() {
        let dot = Graph::empty().to_dot();
        assert!(!dot.is_empty());
        assert!(dot.starts_with("digraph {"));
        assert!(dot.ends_with("}\n"));
    }
}
