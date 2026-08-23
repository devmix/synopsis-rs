//! Domain-bounded BFS traversal (task 1.4, design D4).
//!
//! Oracle mapping: `../synopsis/internal/graph/traverser.go` (`BFS`,
//! `BFSOptions`/`ApplyDefaults`, `matchesRelationType`) — the contract is
//! ported verbatim; the implementation is re-architected on the petgraph
//! `DiGraph` (migration principle: functional copy, not a code copy).
//!
//! ## Contract (D4)
//!
//! [`Graph::traverse`] performs a breadth-first search from a start entity
//! under:
//! - `max_depth` (default 5, hard max 10): BFS levels to expand;
//! - `max_nodes` (default 1000, **start node included**): a hard stop —
//!   once the result holds `max_nodes` nodes the traversal returns;
//! - [`Direction`] { Outgoing, Incoming, Both }, default Both;
//! - `relation_types` filter (empty = no filter);
//! - `follow_entity_links`: the only key to cross-domain steps.
//!
//! **Domain boundary rules** (the oracle's contract):
//! - a **fact** edge to an entity in another domain is **never** traversed,
//!   regardless of `follow_entity_links`;
//! - an **entity-link** edge to another domain is traversed **only** when
//!   `follow_entity_links` is true.
//!
//! The result ([`TraverseResult`]) holds the center entity, all discovered
//! nodes (center first, then discovery order) and exactly one edge per
//! discovered non-center node — the edge that led to it — so
//! `edges.len() == nodes.len() - 1 <= nodes.len()` (the oracle's
//! `len(Edges) <= len(Nodes)`).
//!
//! ## Determinism (pinned)
//!
//! The result does not depend on hash order, DB row order or edge insertion
//! order:
//! - within a BFS level, nodes are expanded in **ascending entity id**
//!   order (the oracle's sorted levels);
//! - for each node, candidate edges are expanded **fact edges first**
//!   (sorted by source id, target id, relation type), then **entity-link
//!   edges** (same key). Fact-first keeps the fact edge's provenance when a
//!   pair has both a fact edge and a link (the oracle's m-12 behavior);
//!   ties on the same (source, target) pair break by relation type, so the
//!   winner never depends on DB row order.
//!
//! ## Conscious deviations from the oracle
//!
//! - **Cross-domain fact edge with a link between the pair.** The oracle's
//!   boundary check (`HasEntityLinkBetween`) allowed a cross-domain FACT
//!   edge to be traversed when an entity link happened to exist between the
//!   pair — contradicting its own comment ("Fact edges crossing domains are
//!   never traversable even with FollowEntityLinks == true") and the D4
//!   contract ("unconditional rule"). Here the edge KIND decides: a
//!   cross-domain fact edge is always blocked; the neighbor is discovered
//!   via the link edge instead (with link provenance in the result).
//! - **Numeric sort keys.** The oracle sorted candidate edges by the
//!   lexicographic string `"src-tgt"` (multi-digit ids misorder:
//!   `"10-2" < "2-1"`). Here: numeric `(source id, target id, relation
//!   type)`.
//! - **No cancellation.** The oracle's `context.Context` is a Go idiom; the
//!   Rust API is synchronous (callers wrap it in `spawn_blocking` where
//!   cancellation is needed).
//! - **Edge endpoints in the result.** Petgraph edge references are
//!   internal, so [`TraversalEdge`] carries entity row ids (the oracle's
//!   `SourceID`/`TargetID`).
//!
//! Task 1.8 (`path_exists`) reuses the same boundary rule.

use std::collections::HashSet;

use petgraph::Direction as PetDirection;
use petgraph::graph::{DiGraph, EdgeIndex, NodeIndex};
use petgraph::visit::EdgeRef;

use crate::error::GraphError;
use crate::graph::{EdgeKind, EntityNode, Graph, GraphEdge};

/// Default maximum depth (the oracle's `ApplyDefaults`).
pub const DEFAULT_MAX_DEPTH: u32 = 5;
/// Hard maximum depth (the oracle's `ApplyDefaults`).
pub const HARD_MAX_DEPTH: u32 = 10;
/// Default maximum number of result nodes, start included (the oracle's
/// `ApplyDefaults`).
pub const DEFAULT_MAX_NODES: usize = 1000;

/// Which edges to follow during traversal (the oracle's `Direction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    /// Follow only edges leaving each expanded node.
    Outgoing,
    /// Follow only edges entering each expanded node.
    Incoming,
    /// Follow both (the default; the oracle's `"both"`).
    #[default]
    Both,
}

/// Traversal parameters (the oracle's `BFSOptions`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TraverseOptions {
    /// Maximum BFS depth; `0` → [`DEFAULT_MAX_DEPTH`], values above
    /// [`HARD_MAX_DEPTH`] are clamped to it.
    pub max_depth: u32,
    /// Edge direction (default [`Direction::Both`]).
    pub direction: Direction,
    /// Relation-type filter; empty = no filter (all types pass).
    pub relation_types: Vec<String>,
    /// Maximum number of result nodes, **start node included**; `0` →
    /// [`DEFAULT_MAX_NODES`].
    pub max_nodes: usize,
    /// Allow crossing domain boundaries via entity-link edges (D4).
    pub follow_entity_links: bool,
}

impl TraverseOptions {
    /// Effective parameters after normalization (the oracle's
    /// `ApplyDefaults`): `max_depth` `0` → default, clamped to the hard
    /// max; `max_nodes` `0` → default.
    pub fn normalized(&self) -> Self {
        let max_depth = if self.max_depth == 0 {
            DEFAULT_MAX_DEPTH
        } else {
            self.max_depth.min(HARD_MAX_DEPTH)
        };
        let max_nodes = if self.max_nodes == 0 {
            DEFAULT_MAX_NODES
        } else {
            self.max_nodes
        };
        Self {
            max_depth,
            max_nodes,
            direction: self.direction,
            relation_types: self.relation_types.clone(),
            follow_entity_links: self.follow_entity_links,
        }
    }
}

/// An edge in the traversal result (the oracle's `Edge`): endpoints as
/// entity row ids plus the edge's identity and provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct TraversalEdge {
    /// Source entity row id (the edge's stored direction).
    pub source: i64,
    /// Target entity row id.
    pub target: i64,
    /// Which population this edge belongs to (the D4 boundary key).
    pub kind: EdgeKind,
    /// Fact predicate or link relation type.
    pub relation_type: String,
    /// Entity-link method; `None` for fact edges.
    pub method: Option<String>,
    /// Entity-link confidence; `None` for fact edges.
    pub confidence: Option<f64>,
    /// Entity-link evidence; `None` for fact edges.
    pub evidence: Option<String>,
}

/// The traversal result (the oracle's `GraphResult`).
#[derive(Debug, Clone, PartialEq)]
pub struct TraverseResult {
    /// The center (start) entity.
    pub center: EntityNode,
    /// Discovered nodes: the center first, then in discovery order.
    pub nodes: Vec<EntityNode>,
    /// The edge that led to each discovered non-center node, so
    /// `edges.len() == nodes.len() - 1 <= nodes.len()`.
    pub edges: Vec<TraversalEdge>,
}

impl Graph {
    /// Breadth-first search from `start_entity_id` (the oracle's `BFS`)
    /// with the D4 domain-boundary rules.
    ///
    /// See the module docs for the full contract and the pinned
    /// determinism order. [`GraphError::EntityNotFound`] if the start
    /// entity is not in the index.
    pub fn traverse(
        &self,
        start_entity_id: i64,
        options: &TraverseOptions,
    ) -> Result<TraverseResult, GraphError> {
        let opts = options.normalized();
        let start = self
            .node_index(start_entity_id)
            .ok_or(GraphError::EntityNotFound {
                entity_id: start_entity_id,
            })?;
        let Some(center) = self.node(start) else {
            // The index and the graph are built together (task 1.2); an
            // index entry without a node weight cannot occur.
            unreachable!("node index resolved from the same graph");
        };
        let center = center.clone();
        let start_domain = center.domain.clone();

        let g = self.graph();
        let mut result = TraverseResult {
            center: center.clone(),
            nodes: vec![center],
            edges: Vec::new(),
        };
        let mut visited: HashSet<NodeIndex> = HashSet::with_capacity(opts.max_nodes);
        visited.insert(start);
        let mut level: Vec<NodeIndex> = vec![start];

        for _depth in 0..opts.max_depth {
            if level.is_empty() {
                break;
            }
            let mut next_level = Vec::new();
            for &node in &level {
                for (edge, from, to) in self.candidate_edges(node, &opts) {
                    let weight = &g[edge];
                    if !relation_type_allowed(&weight.relation_type, &opts.relation_types) {
                        continue;
                    }
                    let neighbor = if from == node { to } else { from };
                    if visited.contains(&neighbor) {
                        continue; // cycle protection
                    }
                    if result.nodes.len() >= opts.max_nodes {
                        return Ok(result); // hard node limit
                    }
                    let neighbor_domain = &g[neighbor].domain;
                    if neighbor_domain != &start_domain
                        && (weight.kind != EdgeKind::EntityLink || !opts.follow_entity_links)
                    {
                        // D4 boundary: fact edges never cross domains;
                        // entity links only with the flag.
                        continue;
                    }
                    visited.insert(neighbor);
                    result.nodes.push(g[neighbor].clone());
                    result.edges.push(TraversalEdge {
                        source: g[from].id,
                        target: g[to].id,
                        kind: weight.kind,
                        relation_type: weight.relation_type.clone(),
                        method: weight.method.clone(),
                        confidence: weight.confidence,
                        evidence: weight.evidence.clone(),
                    });
                    next_level.push(neighbor);
                }
            }
            // Pinned level order: ascending entity id (module docs).
            next_level.sort_by_key(|i| g[*i].id);
            level = next_level;
        }
        Ok(result)
    }

    /// Candidate edges to expand from `node` under `opts`, in the pinned
    /// deterministic order (module docs): fact edges sorted by (source id,
    /// target id, relation type), then entity-link edges the same.
    fn candidate_edges(
        &self,
        node: NodeIndex,
        opts: &TraverseOptions,
    ) -> Vec<(EdgeIndex, NodeIndex, NodeIndex)> {
        let g = self.graph();
        let mut candidates = collect_edges(g, node, opts.direction, EdgeKind::Fact);
        sort_candidates(g, &mut candidates);
        if opts.follow_entity_links {
            let mut links = collect_edges(g, node, opts.direction, EdgeKind::EntityLink);
            sort_candidates(g, &mut links);
            candidates.extend(links);
        }
        candidates
    }
}

/// Incident edges of `node` in `direction` of the given `kind`, as
/// `(edge, source, target)` triples (petgraph iteration order — the
/// caller sorts for the pinned determinism order).
fn collect_edges(
    graph: &DiGraph<EntityNode, GraphEdge>,
    node: NodeIndex,
    direction: Direction,
    kind: EdgeKind,
) -> Vec<(EdgeIndex, NodeIndex, NodeIndex)> {
    let mut out: Vec<(EdgeIndex, NodeIndex, NodeIndex)> = Vec::new();
    let mut add = |dir: PetDirection| {
        for edge in graph.edges_directed(node, dir) {
            if edge.weight().kind != kind {
                continue;
            }
            out.push((edge.id(), edge.source(), edge.target()));
        }
    };
    match direction {
        Direction::Outgoing => add(PetDirection::Outgoing),
        Direction::Incoming => add(PetDirection::Incoming),
        Direction::Both => {
            add(PetDirection::Outgoing);
            add(PetDirection::Incoming);
        }
    }
    out
}

/// Pinned candidate order: (source id, target id, relation type) ascending.
fn sort_candidates(
    graph: &DiGraph<EntityNode, GraphEdge>,
    edges: &mut [(EdgeIndex, NodeIndex, NodeIndex)],
) {
    edges.sort_by_key(|(edge, from, to)| {
        (
            graph[*from].id,
            graph[*to].id,
            graph[*edge].relation_type.as_str(),
        )
    });
}

/// The oracle's `matchesRelationType`: an empty filter passes everything.
fn relation_type_allowed(relation_type: &str, allowed: &[String]) -> bool {
    allowed.is_empty() || allowed.iter().any(|t| t == relation_type)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use db::{Entity, EntityLink, Fact};

    // ── fixtures ────────────────────────────────────────────────────────
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

    /// Two-domain fixture.
    ///
    /// hr: Alice(1) →Bob(2) →Carol(3), Carol→Alice, Alice→Acme (CROSS-domain
    /// fact). it: Acme(4), Globex(5). Links: Alice→Globex, Acme→Alice.
    fn two_domain_graph() -> Graph {
        let entities = vec![
            entity(1, "PERSON", "Alice", "hr"),
            entity(2, "PERSON", "Bob", "hr"),
            entity(3, "PERSON", "Carol", "hr"),
            entity(4, "ORGANIZATION", "Acme", "it"),
            entity(5, "ORGANIZATION", "Globex", "it"),
        ];
        let facts = vec![
            fact(1, 1, "knows", 2),
            fact(2, 2, "knows", 3),
            fact(3, 3, "reports_to", 1),
            fact(4, 1, "works_at", 4),
        ];
        let links = vec![
            link(
                1,
                5,
                "same_entity",
                "rule",
                0.95,
                Some("rule: hr/Alice -> it/Globex"),
            ),
            link(4, 1, "related_to", "equals", 0.8, None),
        ];
        Graph::from_rows(entities, facts, links)
    }

    /// Cross-domain pair with ONLY a fact edge (no link between the pair).
    fn fact_only_cross_graph() -> Graph {
        let entities = vec![
            entity(1, "PERSON", "Alice", "hr"),
            entity(2, "PERSON", "Bob", "hr"),
            entity(4, "ORGANIZATION", "Acme", "it"),
        ];
        let facts = vec![fact(1, 1, "knows", 2), fact(2, 1, "works_at", 4)];
        Graph::from_rows(entities, facts, Vec::new())
    }

    /// Cross-domain pair with a fact edge A→D AND a link D→A (the oracle
    /// bug-fix case: the fact edge must stay blocked, D is reached via the
    /// link).
    fn fact_plus_link_cross_graph() -> Graph {
        let entities = vec![
            entity(1, "PERSON", "Alice", "hr"),
            entity(4, "ORGANIZATION", "Acme", "it"),
        ];
        let facts = vec![fact(1, 1, "works_at", 4)];
        let links = vec![link(4, 1, "related_to", "equals", 0.8, None)];
        Graph::from_rows(entities, facts, links)
    }

    /// Straight in-domain chain A(1)→B(2)→C(3)→D(4) for depth/node limits.
    fn chain_graph() -> Graph {
        let entities = (1..=4)
            .map(|id| entity(id, "PERSON", &format!("N{id}"), "hr"))
            .collect();
        let facts = vec![
            fact(1, 1, "next", 2),
            fact(2, 2, "next", 3),
            fact(3, 3, "next", 4),
        ];
        Graph::from_rows(entities, facts, Vec::new())
    }

    // ── assertion helpers ───────────────────────────────────────────────────

    fn node_ids(result: &TraverseResult) -> Vec<i64> {
        result.nodes.iter().map(|n| n.id).collect()
    }

    fn edge_keys(result: &TraverseResult) -> Vec<(i64, i64, String)> {
        result
            .edges
            .iter()
            .map(|e| (e.source, e.target, e.relation_type.clone()))
            .collect()
    }

    /// Result invariants: the center is the first node, and exactly one edge
    /// per discovered non-center node (`len(edges) == len(nodes) - 1`).
    fn assert_invariants(result: &TraverseResult) {
        assert_eq!(
            result.nodes.first().map(|n| n.id),
            Some(result.center.id),
            "the center must be the first node"
        );
        assert_eq!(
            result.edges.len(),
            result.nodes.len().saturating_sub(1),
            "one edge per discovered non-center node (len(edges) <= len(nodes))"
        );
    }

    fn options(follow_entity_links: bool) -> TraverseOptions {
        TraverseOptions {
            follow_entity_links,
            ..Default::default()
        }
    }

    // ── domain boundary (the core D4 contract) ──────────────────────────────

    // Acceptance: a cross-domain FACT edge is blocked under BOTH flag values
    // (the oracle's `TestBFSWithFollowEntityLinks_FactEdgeNoCrossDomain`).
    #[test]
    fn fact_boundary_blocked_under_both_flag_values() {
        let graph = fact_only_cross_graph();
        for follow in [false, true] {
            let result = graph.traverse(1, &options(follow)).unwrap();
            assert_invariants(&result);
            assert_eq!(
                node_ids(&result),
                vec![1, 2],
                "Acme is reachable only via the cross-domain fact edge; follow={follow}"
            );
        }
    }

    // Acceptance: an entity-link crossing is allowed ONLY when
    // follow_entity_links=true (the oracle's
    // `TestBFSWithoutFollowEntityLinks_SameDomainOnly` /
    // `TestBFSWithFollowEntityLinks_CrossDomainViaLinks`).
    #[test]
    fn entity_link_crossing_allowed_per_flag() {
        let graph = two_domain_graph();

        let without = graph.traverse(1, &options(false)).unwrap();
        assert_invariants(&without);
        assert_eq!(node_ids(&without), vec![1, 2, 3]);
        assert_eq!(
            edge_keys(&without),
            vec![(1, 2, "knows".into()), (3, 1, "reports_to".into())]
        );

        let with = graph.traverse(1, &options(true)).unwrap();
        assert_invariants(&with);
        // Globex via the outgoing link, Acme via the INCOMING link.
        assert_eq!(node_ids(&with), vec![1, 2, 3, 5, 4]);
        assert_eq!(
            edge_keys(&with),
            vec![
                (1, 2, "knows".into()),
                (3, 1, "reports_to".into()),
                (1, 5, "same_entity".into()),
                (4, 1, "related_to".into()),
            ]
        );
        // Both cross-domain edges in the result are EntityLink kind.
        for edge in &with.edges {
            if edge.source == 5 || edge.target == 5 || edge.source == 4 || edge.target == 4 {
                assert_eq!(edge.kind, EdgeKind::EntityLink);
            }
        }
    }

    // Conscious deviation (oracle bug fix): a cross-domain fact edge stays
    // blocked even when an entity link exists between the pair (the oracle's
    // `HasEntityLinkBetween` check allowed it — against its own comment and
    // the D4 "unconditional rule"). The neighbor is reached via the LINK edge.
    #[test]
    fn cross_domain_fact_blocked_even_when_link_exists() {
        let graph = fact_plus_link_cross_graph();

        let with = graph.traverse(1, &options(true)).unwrap();
        assert_invariants(&with);
        assert_eq!(node_ids(&with), vec![1, 4], "Acme is reached via the link");
        assert_eq!(edge_keys(&with), vec![(4, 1, "related_to".into())]);
        assert_eq!(
            with.edges[0].kind,
            EdgeKind::EntityLink,
            "the fact edge (1,4) must not appear in the result"
        );

        let without = graph.traverse(1, &options(false)).unwrap();
        assert_invariants(&without);
        assert_eq!(
            node_ids(&without),
            vec![1],
            "no link traversal without the flag"
        );
    }

    // The oracle's m-12 behavior, kept: for a pair that has BOTH a fact edge
    // and a link (same domain), the fact edge's provenance wins.
    #[test]
    fn fact_provenance_wins_for_dual_pair() {
        let entities = vec![
            entity(1, "PERSON", "Alice", "hr"),
            entity(2, "PERSON", "Bob", "hr"),
        ];
        let facts = vec![fact(1, 1, "knows", 2)];
        let links = vec![link(1, 2, "same_entity", "rule", 0.9, None)];
        let graph = Graph::from_rows(entities, facts, links);

        let result = graph.traverse(1, &options(true)).unwrap();
        assert_invariants(&result);
        assert_eq!(node_ids(&result), vec![1, 2]);
        assert_eq!(edge_keys(&result), vec![(1, 2, "knows".into())]);
        assert_eq!(
            result.edges[0].kind,
            EdgeKind::Fact,
            "fact edge provenance preserved"
        );
    }

    // ── directions ──────────────────────────────────────────────────────────

    // Acceptance: all three directions give the correct adjacency (the oracle's
    // determinism tests for Outgoing/Incoming + the both-direction contract).
    #[test]
    fn directions_give_correct_adjacency() {
        let graph = two_domain_graph();
        let dir = |d: Direction, follow: bool| TraverseOptions {
            direction: d,
            follow_entity_links: follow,
            ..Default::default()
        };

        // Outgoing: Alice→Bob→Carol; the cross-domain fact is blocked and the
        // INCOMING Acme→Alice link is not followed.
        let out = graph.traverse(1, &dir(Direction::Outgoing, false)).unwrap();
        assert_invariants(&out);
        assert_eq!(node_ids(&out), vec![1, 2, 3]);
        assert_eq!(
            edge_keys(&out),
            vec![(1, 2, "knows".into()), (2, 3, "knows".into())]
        );

        let out_follow = graph.traverse(1, &dir(Direction::Outgoing, true)).unwrap();
        assert_invariants(&out_follow);
        assert_eq!(
            node_ids(&out_follow),
            vec![1, 2, 5, 3],
            "outgoing link followed, incoming not"
        );
        assert_eq!(
            edge_keys(&out_follow),
            vec![
                (1, 2, "knows".into()),
                (1, 5, "same_entity".into()),
                (2, 3, "knows".into())
            ]
        );

        // Incoming: Carol→Alice, Bob→Carol, Acme→Alice (link, with flag).
        let in_ = graph.traverse(1, &dir(Direction::Incoming, false)).unwrap();
        assert_invariants(&in_);
        assert_eq!(node_ids(&in_), vec![1, 3, 2]);
        assert_eq!(
            edge_keys(&in_),
            vec![(3, 1, "reports_to".into()), (2, 3, "knows".into())]
        );

        let in_follow = graph.traverse(1, &dir(Direction::Incoming, true)).unwrap();
        assert_invariants(&in_follow);
        assert_eq!(node_ids(&in_follow), vec![1, 3, 4, 2]);
        assert_eq!(
            edge_keys(&in_follow),
            vec![
                (3, 1, "reports_to".into()),
                (4, 1, "related_to".into()),
                (2, 3, "knows".into()),
            ]
        );

        // Both: the union (covered by the boundary tests as well).
        let both = graph.traverse(1, &dir(Direction::Both, false)).unwrap();
        assert_eq!(node_ids(&both), vec![1, 2, 3]);
        let both_follow = graph.traverse(1, &dir(Direction::Both, true)).unwrap();
        assert_eq!(node_ids(&both_follow), vec![1, 2, 3, 5, 4]);
    }

    // ── limits ──────────────────────────────────────────────────────────────

    // Acceptance: depth and node limits are respected.
    #[test]
    fn depth_and_node_limits_respected() {
        let chain = chain_graph();
        let depth = |d: u32| TraverseOptions {
            max_depth: d,
            ..Default::default()
        };

        assert_eq!(node_ids(&chain.traverse(1, &depth(1)).unwrap()), vec![1, 2]);
        assert_eq!(
            node_ids(&chain.traverse(1, &depth(2)).unwrap()),
            vec![1, 2, 3]
        );
        assert_eq!(
            node_ids(&chain.traverse(1, &depth(3)).unwrap()),
            vec![1, 2, 3, 4]
        );
        // The hard max (100 is clamped to 10) still covers the chain.
        assert_eq!(
            node_ids(&chain.traverse(1, &depth(100)).unwrap()),
            vec![1, 2, 3, 4]
        );

        // max_nodes counts the START node too; edges only between included
        // nodes (the invariant holds in every case).
        let graph = two_domain_graph();
        let nodes = |n: usize| TraverseOptions {
            max_nodes: n,
            follow_entity_links: true,
            ..Default::default()
        };
        let only_center = graph.traverse(1, &nodes(1)).unwrap();
        assert_invariants(&only_center);
        assert_eq!(node_ids(&only_center), vec![1]);

        let two = graph.traverse(1, &nodes(2)).unwrap();
        assert_invariants(&two);
        assert_eq!(node_ids(&two), vec![1, 2]);

        // Without the limit Globex (5) is reached; with max_nodes=3 the
        // traversal stops before it.
        let three = graph.traverse(1, &nodes(3)).unwrap();
        assert_invariants(&three);
        assert_eq!(node_ids(&three), vec![1, 2, 3]);
        assert!(
            !node_ids(&three).contains(&5),
            "the limit must cut the traversal before Globex"
        );
        assert!(node_ids(&graph.traverse(1, &nodes(1000)).unwrap()).contains(&5));
    }

    // ── determinism ─────────────────────────────────────────────────────────

    // Acceptance: two runs are identical (full result equality: node order,
    // edge order, provenance) across directions and flag values.
    #[test]
    fn two_runs_are_identical() {
        let graph = two_domain_graph();
        for direction in [Direction::Outgoing, Direction::Incoming, Direction::Both] {
            for follow in [false, true] {
                let opts = TraverseOptions {
                    direction,
                    follow_entity_links: follow,
                    ..Default::default()
                };
                let first = graph.traverse(1, &opts).unwrap();
                let second = graph.traverse(1, &opts).unwrap();
                assert_eq!(
                    first, second,
                    "direction={direction:?} follow={follow} must be deterministic"
                );
                assert_invariants(&first);
            }
        }
    }

    // ── errors, normalization, filters, degenerate graphs ───────────────────

    // Acceptance: an unknown start entity is an explicit error (the oracle's
    // "start node %d not found").
    #[test]
    fn unknown_start_entity_is_an_error() {
        let graph = two_domain_graph();
        let err = graph
            .traverse(999, &TraverseOptions::default())
            .unwrap_err();
        assert!(
            matches!(err, GraphError::EntityNotFound { entity_id: 999 }),
            "got {err:?}"
        );
    }

    // Acceptance: option normalization (the oracle's zero value +
    // `ApplyDefaults`): `Default` is the zero value, `normalized()` fills the
    // effective defaults.
    #[test]
    fn options_normalize_like_the_oracle() {
        let def = TraverseOptions::default();
        assert_eq!(
            def.max_depth, 0,
            "zero value, like the oracle's zero-valued BFSOptions"
        );
        assert_eq!(def.max_nodes, 0);
        assert_eq!(def.direction, Direction::Both);
        assert!(def.relation_types.is_empty());
        assert!(!def.follow_entity_links);

        let normalized = def.normalized();
        assert_eq!(normalized.max_depth, 5);
        assert_eq!(normalized.max_nodes, 1000);
        assert_eq!(normalized.direction, Direction::Both);

        let big_depth = TraverseOptions {
            max_depth: 15,
            ..Default::default()
        };
        assert_eq!(big_depth.normalized().max_depth, 10, "hard max 10");

        let mid_depth = TraverseOptions {
            max_depth: 7,
            ..Default::default()
        };
        assert_eq!(mid_depth.normalized().max_depth, 7);

        let zero_nodes = TraverseOptions {
            max_nodes: 0,
            ..Default::default()
        };
        assert_eq!(zero_nodes.normalized().max_nodes, 1000);

        let small_nodes = TraverseOptions {
            max_nodes: 50,
            ..Default::default()
        };
        assert_eq!(small_nodes.normalized().max_nodes, 50);
    }

    // The relation-type filter passes only the listed types (empty = all).
    #[test]
    fn relation_type_filter_applies_to_facts_and_links() {
        let graph = two_domain_graph();
        let filter = |types: &[&str]| TraverseOptions {
            relation_types: types.iter().map(|t| t.to_string()).collect(),
            follow_entity_links: true,
            ..Default::default()
        };

        // Only "knows": Carol is reached via Bob, never via "reports_to".
        let knows = graph.traverse(1, &filter(&["knows"])).unwrap();
        assert_invariants(&knows);
        assert_eq!(node_ids(&knows), vec![1, 2, 3]);
        assert_eq!(
            edge_keys(&knows),
            vec![(1, 2, "knows".into()), (2, 3, "knows".into())]
        );

        // Only "reports_to": Carol is the sole neighbor of Alice.
        let reports = graph.traverse(1, &filter(&["reports_to"])).unwrap();
        assert_invariants(&reports);
        assert_eq!(node_ids(&reports), vec![1, 3]);

        // A type no edge carries: only the center.
        let works = graph.traverse(1, &filter(&["works_at"])).unwrap();
        assert_invariants(&works);
        assert_eq!(node_ids(&works), vec![1]);
    }

    // Degenerate graphs: empty index → error; a single isolated node → a valid
    // one-node result.
    #[test]
    fn empty_and_single_node_graphs() {
        let empty = Graph::empty();
        assert!(matches!(
            empty.traverse(1, &TraverseOptions::default()),
            Err(GraphError::EntityNotFound { entity_id: 1 })
        ));

        let single = Graph::from_rows(
            vec![entity(7, "PERSON", "Solo", "hr")],
            Vec::new(),
            Vec::new(),
        );
        let result = single.traverse(7, &TraverseOptions::default()).unwrap();
        assert_invariants(&result);
        assert_eq!(node_ids(&result), vec![7]);
        assert!(result.edges.is_empty());
    }

    // Entity-link provenance (method/confidence/evidence) survives into the
    // result (the oracle's m-10).
    #[test]
    fn entity_link_provenance_preserved() {
        let graph = two_domain_graph();
        let result = graph.traverse(1, &options(true)).unwrap();
        let edge = result
            .edges
            .iter()
            .find(|e| e.source == 1 && e.target == 5)
            .expect("the Alice→Globex link edge is in the result");
        assert_eq!(edge.kind, EdgeKind::EntityLink);
        assert_eq!(edge.method.as_deref(), Some("rule"));
        assert_eq!(edge.confidence, Some(0.95));
        assert_eq!(
            edge.evidence.as_deref(),
            Some("rule: hr/Alice -> it/Globex")
        );

        let fact_edge = result
            .edges
            .iter()
            .find(|e| e.source == 1 && e.target == 2)
            .expect("the Alice→Bob fact edge is in the result");
        assert_eq!(fact_edge.kind, EdgeKind::Fact);
        assert_eq!(fact_edge.method, None);
        assert_eq!(fact_edge.confidence, None);
        assert_eq!(fact_edge.evidence, None);
    }
}
