//! Index builder: SQLite → in-memory `petgraph::DiGraph` (design D1/D3).
//!
//! Oracle mapping: `../synopsis/internal/graph/graph.go` (`NewGraphFromDB`,
//! `loadEntities`, `loadRelations`, `loadEntityLinks`) — functional copy,
//! re-architected for Rust (migration principle: not a code copy).
//!
//! ## Storage model (D1)
//!
//! The `entities` / `facts` / `entity_links` tables in SQLite (via the `db`
//! crate DAOs) are the single source of truth. [`Graph`] is a derived,
//! read-only in-memory index built in ONE pass ([`Graph::from_rows`], D3 —
//! full rebuild, incrementality is YAGNI at personal-corpus scale) and never
//! written back.
//!
//! The oracle's eight hand-rolled maps collapse into one `DiGraph` plus two
//! `HashMap` indexes:
//! - [`Graph::find_exact`] key `"domain:lowercase_name"` → node, O(1)
//!   (oracle `nameToID`, same key format);
//! - [`Graph::nodes_by_type`] — entity type → node indexes
//!   (oracle `typeToNodes`).
//!
//! ## Fact edges vs entity-link edges (oracle verification, task 1.2)
//!
//! Verified against `../synopsis/internal/graph/graph.go` + `traverser.go`:
//!
//! - **Facts are materialized as directed edges.** `loadRelations` reads
//!   every fact and adds it as an edge `subject → object` with the fact
//!   `predicate` as the relation type. Nothing is computed at traversal
//!   time — facts become edges at load.
//! - **Entity links are a second edge population**, kept in separate
//!   adjacency structures (`outgoing/incomingCrossDomainEdges`) plus an
//!   `entityLinkTargetSet` for O(1) bidirectional existence checks.
//! - **The traverser distinguishes the two kinds** (`traverser.go`): fact
//!   edges **never** cross domain boundaries (even with
//!   `FollowEntityLinks=true`); a cross-domain step is allowed only through
//!   an entity-link edge and only when `FollowEntityLinks=true` and an
//!   entity link exists between the two nodes.
//!
//! Rust equivalent: both populations are edges of the single `DiGraph`,
//! tagged by [`EdgeKind`] in the edge weight. The D4 domain-boundary rule is
//! applied at expansion time by the traverser (task 1.4) using the tag plus
//! node domains. The oracle's `entityLinkTargetSet` maps to petgraph's
//! `find_edge(a, b)` / `find_edge(b, a)` (O(degree) — irrelevant at personal
//! corpus scale).
//!
//! ## Conscious deviations from the oracle
//!
//! - Edge endpoints are NOT stored in the edge weight: petgraph edge
//!   references are authoritative (the oracle's `Edge` struct duplicated
//!   `SourceID`/`TargetID`).
//! - Fact edges with a `NULL` endpoint (v5 schema allows it) or an endpoint
//!   missing from `entities` are skipped at build time: petgraph cannot
//!   attach an edge to a non-existent node (the oracle stored such edges
//!   against node id 0 and the traverser skipped them as dangling).
//! - The name-index key uses the `db` crate's `normalize` (trim + collapse +
//!   lowercase) for BOTH domain and name — a superset of the oracle's
//!   `ToLower`-only name key, consistent with `EntityDao::get_by_name_fold`.
//! - On a name-key collision (two types share name+domain) the SMALLEST
//!   entity id wins (entities are processed in id order); the oracle's
//!   last-write-wins entry depended on `List()` order. Smallest-id matches
//!   the oracle's own cross-domain lookup (lowest matching id).
//! - Fact edge `domain`/`metadata` are not carried in the edge weight: the
//!   oracle's traverser never reads them (boundary checks use node domains).
//! - The oracle's `avgDegree` double-counts every edge (outgoing + incoming)
//!   — a Go bug; task 1.5 computes `2*E/N`.
//!
//! ## Flag semantics (D13, config crate)
//!
//! `enable_graph=false` or `load_on_startup=false` → no index is built and
//! [`GraphIndex::Unavailable`] is returned — an explicit valid state, NOT an
//! error. An empty database yields an empty, valid index.

use std::collections::HashMap;

use config::preset::GraphConfig;
use db::{
    ConnectionOrTx, Db, DbError, Entity, EntityDao, EntityLink, EntityLinkDao, Fact, FactDao,
    utils::normalize,
};
use petgraph::graph::{DiGraph, EdgeIndex, NodeIndex};

use crate::error::GraphError;

/// One node of the in-memory graph index: an entity row of the `entities`
/// table.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityNode {
    /// The entity row id (stable identity across rebuilds).
    pub id: i64,
    /// Entity type (`entities.type`).
    pub entity_type: String,
    /// Canonical name as stored (display form).
    pub name: String,
    /// Domain, normalized (trim + collapse + lowercase); the D4 boundary
    /// rule compares these values.
    pub domain: String,
}

/// The population an edge belongs to (see the module docs for the oracle
/// verification).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// A fact edge: `subject → object` from the `facts` table; the fact
    /// predicate is the relation type. Never crosses domains (D4).
    Fact,
    /// A cross-domain link from the `entity_links` table; the only edge kind
    /// that may cross a domain boundary (D4, `FollowEntityLinks`).
    EntityLink,
}

/// Edge weight: the relation plus its provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphEdge {
    /// Which population this edge belongs to (the D4 boundary key).
    pub kind: EdgeKind,
    /// Fact edges: the fact predicate; entity links: the link relation type.
    pub relation_type: String,
    /// Entity-link method (`'rule'`/`'equals'`/`'llm'`); `None` for facts.
    pub method: Option<String>,
    /// Entity-link confidence; `None` for facts.
    pub confidence: Option<f64>,
    /// Entity-link evidence; `None` for facts.
    pub evidence: Option<String>,
}

/// The in-memory graph index (D1): a `petgraph::DiGraph` plus O(1) lookup
/// indexes. Read-only derived data — built once per rebuild (D3), never
/// written back.
#[derive(Debug)]
pub struct Graph {
    /// Directed graph: nodes = entities, edges = fact + entity-link edges.
    /// Parallel edges are allowed (one pair may have several relation types).
    graph: DiGraph<EntityNode, GraphEdge>,
    /// Entity row id → node index (entity ids are i64 rowids; petgraph
    /// indices are u32).
    by_entity_id: HashMap<i64, NodeIndex>,
    /// `"domain:lowercase_name"` → node index; O(1) exact lookup (oracle
    /// `nameToID`, same key format).
    by_name: HashMap<String, NodeIndex>,
    /// Entity type → node indexes in entity-id order (oracle `typeToNodes`).
    by_type: HashMap<String, Vec<NodeIndex>>,
}

impl Graph {
    /// An empty, valid index (an empty database yields exactly this).
    pub fn empty() -> Self {
        Self {
            graph: DiGraph::new(),
            by_entity_id: HashMap::new(),
            by_name: HashMap::new(),
            by_type: HashMap::new(),
        }
    }

    /// Build the index in one pass from entity, fact and entity-link rows
    /// (D3). Pure: no I/O, so the builder is unit-testable without SQLite.
    ///
    /// Nodes are created in entity-id order, so index contents (including
    /// name-collision resolution) never depend on row order.
    pub fn from_rows(entities: Vec<Entity>, facts: Vec<Fact>, links: Vec<EntityLink>) -> Self {
        let mut graph = DiGraph::with_capacity(entities.len(), facts.len() + links.len());
        let mut by_entity_id = HashMap::with_capacity(entities.len());
        let mut by_name = HashMap::with_capacity(entities.len());
        let mut by_type: HashMap<String, Vec<NodeIndex>> = HashMap::new();

        let mut entities = entities;
        entities.sort_by_key(|e| e.id);
        for entity in entities {
            let domain = normalize(&entity.domain);
            let name_key = normalize(&entity.name);
            let key = format!("{domain}:{name_key}");
            let type_key = entity.entity_type.clone();
            let index = graph.add_node(EntityNode {
                id: entity.id,
                entity_type: entity.entity_type,
                name: entity.name,
                domain,
            });
            by_entity_id.insert(entity.id, index);
            // Smallest id wins on a collision (module docs).
            by_name.entry(key).or_insert(index);
            by_type.entry(type_key).or_default().push(index);
        }

        for fact in facts {
            let (Some(subject), Some(object)) = (fact.subject_entity_id, fact.object_entity_id)
            else {
                continue; // NULL endpoint: no node to attach the edge to
            };
            let (Some(from), Some(to)) = (
                by_entity_id.get(&subject).copied(),
                by_entity_id.get(&object).copied(),
            ) else {
                continue; // dangling endpoint: not present in the index
            };
            graph.add_edge(
                from,
                to,
                GraphEdge {
                    kind: EdgeKind::Fact,
                    relation_type: fact.predicate,
                    method: None,
                    confidence: None,
                    evidence: None,
                },
            );
        }

        for link in links {
            let (Some(from), Some(to)) = (
                by_entity_id.get(&link.subject_entity_id).copied(),
                by_entity_id.get(&link.target_entity_id).copied(),
            ) else {
                continue;
            };
            graph.add_edge(
                from,
                to,
                GraphEdge {
                    kind: EdgeKind::EntityLink,
                    relation_type: link.relation_type,
                    method: Some(link.method),
                    confidence: Some(link.confidence),
                    evidence: link.evidence,
                },
            );
        }

        Self {
            graph,
            by_entity_id,
            by_name,
            by_type,
        }
    }

    /// Number of nodes (entities) in the index.
    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    /// Number of edges (fact edges + entity-link edges) in the index.
    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }

    /// The underlying directed graph (traversal, DOT export).
    pub fn graph(&self) -> &DiGraph<EntityNode, GraphEdge> {
        &self.graph
    }

    /// Node weight at `index`, if present.
    pub fn node(&self, index: NodeIndex) -> Option<&EntityNode> {
        self.graph.node_weight(index)
    }

    /// Edge weight at `index`, if present.
    pub fn edge(&self, index: EdgeIndex) -> Option<&GraphEdge> {
        self.graph.edge_weight(index)
    }

    /// The node index of the entity with row id `entity_id`, if present.
    pub fn node_index(&self, entity_id: i64) -> Option<NodeIndex> {
        self.by_entity_id.get(&entity_id).copied()
    }

    /// O(1) exact name lookup (case- and surrounding-whitespace-insensitive
    /// on both parts): the node index of the entity with the given name in
    /// the given domain, or `None` if absent.
    pub fn find_exact(&self, name: &str, domain: &str) -> Option<NodeIndex> {
        self.by_name
            .get(&format!("{}:{}", normalize(domain), normalize(name)))
            .copied()
    }

    /// All node indexes of `entity_type` in entity-id order; empty if the
    /// type is absent.
    pub fn nodes_by_type(&self, entity_type: &str) -> &[NodeIndex] {
        self.by_type
            .get(entity_type)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

/// The graph index in one of its two states (task 1.2 flag semantics):
/// built and ready, or explicitly unavailable by configuration.
#[derive(Debug)]
pub enum GraphIndex {
    /// A built index (possibly empty: an empty database yields an empty,
    /// valid index).
    Ready(Graph),
    /// The graph module is disabled by configuration
    /// (`enable_graph=false` or `load_on_startup=false`): no index was
    /// built. This is a valid state, **not** an error.
    Unavailable,
}

impl GraphIndex {
    /// Build the index from SQLite per the graph configuration (D13 flag
    /// semantics from the config crate): `enable_graph=false` or
    /// `load_on_startup=false` → [`Self::Unavailable`] (no I/O at all);
    /// otherwise a [`Self::Ready`] index over all entities, approved facts
    /// (`FactDao::list_all`) and entity links, in one pass (D3).
    pub fn from_db(db: &Db, config: &GraphConfig) -> Result<Self, GraphError> {
        if !config.enable_graph || !config.load_on_startup {
            return Ok(Self::Unavailable);
        }
        let graph = db.with_conn(|conn| -> Result<Graph, DbError> {
            let exec = ConnectionOrTx::Connection(conn);
            let entities = EntityDao::new(exec).list()?;
            let facts = FactDao::new(exec).list_all()?;
            let links = EntityLinkDao::new(exec).list_all()?;
            Ok(Graph::from_rows(entities, facts, links))
        })??;
        Ok(Self::Ready(graph))
    }

    /// Whether a built index is available.
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Ready(_))
    }

    /// The built index, if available.
    pub fn ready(&self) -> Option<&Graph> {
        match self {
            Self::Ready(graph) => Some(graph),
            Self::Unavailable => None,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use db::test_util::in_memory_db;

    /// Insert an entity through the DAO and return its id.
    fn seed_entity(db: &Db, entity_type: &str, name: &str, domain: &str) -> i64 {
        db.with_conn(|conn| {
            EntityDao::new(ConnectionOrTx::Connection(conn)).create(
                entity_type,
                name,
                domain,
                None,
                None,
                None,
            )
        })
        .unwrap()
        .unwrap()
    }

    /// Insert an approved fact through the DAO and return its id.
    fn seed_fact(db: &Db, subject: Option<i64>, predicate: &str, object: Option<i64>) -> i64 {
        db.with_conn(|conn| {
            FactDao::new(ConnectionOrTx::Connection(conn))
                .create(subject, predicate, object, "", None, None, None)
        })
        .unwrap()
        .unwrap()
    }

    /// Insert an entity link through the DAO.
    fn seed_link(db: &Db, subject: i64, target: i64, relation_type: &str) {
        let link = EntityLink {
            subject_entity_id: subject,
            target_entity_id: target,
            relation_type: relation_type.into(),
            method: "rule".into(),
            confidence: 0.9,
            evidence: None,
        };
        assert!(
            db.with_conn(|conn| {
                EntityLinkDao::new(ConnectionOrTx::Connection(conn)).create(&link)
            })
            .unwrap()
            .unwrap(),
            "new link must insert"
        );
    }

    /// Node weights in node-index order + edge weights in edge-index order:
    /// a deterministic snapshot for index equivalence.
    fn snapshot(g: &Graph) -> (Vec<EntityNode>, Vec<GraphEdge>) {
        let nodes = g
            .graph()
            .node_indices()
            .map(|i| g.node(i).unwrap().clone())
            .collect();
        let edges = g
            .graph()
            .edge_indices()
            .map(|i| g.edge(i).unwrap().clone())
            .collect();
        (nodes, edges)
    }

    // Acceptance: load N entities / M links → exact node/edge counts, and
    // the two edge populations are distinguishable by kind.
    #[test]
    fn from_db_loads_nodes_and_edges_exactly() {
        let db = in_memory_db();
        let a = seed_entity(&db, "PERSON", "Alice", "hr");
        let b = seed_entity(&db, "PERSON", "Bob", "hr");
        let c = seed_entity(&db, "ORGANIZATION", "Acme", "it");
        seed_fact(&db, Some(a), "works_at", Some(c));
        seed_fact(&db, Some(b), "knows", Some(a));
        seed_link(&db, a, c, "same_entity");
        seed_link(&db, b, c, "related_to");

        let index = GraphIndex::from_db(&db, &GraphConfig::default()).unwrap();
        assert!(index.is_available());
        let g = index.ready().unwrap();
        assert_eq!(g.node_count(), 3, "three entities → three nodes");
        assert_eq!(g.edge_count(), 4, "two facts + two links → four edges");

        let mut fact_edges = 0;
        let mut link_edges = 0;
        for edge in g.graph().edge_weights() {
            match edge.kind {
                EdgeKind::Fact => fact_edges += 1,
                EdgeKind::EntityLink => link_edges += 1,
            }
        }
        assert_eq!(fact_edges, 2, "fact edges tagged Fact");
        assert_eq!(link_edges, 2, "link edges tagged EntityLink");

        // Node weight carries id/type/name/domain.
        let node = g.node(g.node_index(a).unwrap()).unwrap();
        assert_eq!(node.id, a);
        assert_eq!(node.entity_type, "PERSON");
        assert_eq!(node.name, "Alice");
        assert_eq!(node.domain, "hr");

        // A fact edge carries the predicate; an entity link carries
        // provenance (the a→c pair has BOTH in parallel).
        let edges: Vec<GraphEdge> = g.graph().edge_weights().cloned().collect();
        let works_at = edges
            .iter()
            .find(|e| e.kind == EdgeKind::Fact && e.relation_type == "works_at")
            .expect("fact edge a→c exists");
        assert_eq!(works_at.method, None);
        assert_eq!(works_at.confidence, None);

        let same_entity = edges
            .iter()
            .find(|e| e.kind == EdgeKind::EntityLink && e.relation_type == "same_entity")
            .expect("entity link a→c exists");
        assert_eq!(same_entity.method.as_deref(), Some("rule"));
        assert_eq!(same_entity.confidence, Some(0.9));
        assert_eq!(same_entity.evidence, None);
    }

    // Acceptance: name→ID index is O(1), case-insensitive (and
    // surrounding-whitespace-insensitive per the db fold convention),
    // domain-scoped, misses → None.
    #[test]
    fn name_index_is_case_insensitive_and_domain_scoped() {
        let db = in_memory_db();
        let a = seed_entity(&db, "PERSON", "Alice Smith", "hr");
        seed_entity(&db, "PERSON", "Alice Smith", "it"); // same name, other domain

        let index = GraphIndex::from_db(&db, &GraphConfig::default()).unwrap();
        let g = index.ready().unwrap();

        for (name, domain) in [
            ("Alice Smith", "hr"),
            ("alice smith", "hr"),
            ("ALICE SMITH", "HR"),
            ("  alice   smith  ", " hr "),
        ] {
            let found = g.find_exact(name, domain);
            assert_eq!(
                found,
                Some(g.node_index(a).unwrap()),
                "query {name:?} in {domain:?}"
            );
        }
        // The same name in another domain resolves to THAT domain's node…
        let it_node = g
            .nodes_by_type("PERSON")
            .iter()
            .find(|i| g.node(**i).unwrap().domain == "it")
            .expect("the it-domain entity exists");
        assert_eq!(g.find_exact("Alice Smith", "it"), Some(*it_node));
        // …and a domain without the name misses.
        assert_eq!(
            g.find_exact("Alice Smith", "geo"),
            None,
            "absent domain must miss"
        );
        assert_eq!(g.find_exact("Nobody", "hr"), None, "unknown name → None");
    }

    // Acceptance: type→nodes index groups by type, in entity-id order.
    #[test]
    fn type_index_groups_nodes_in_id_order() {
        let db = in_memory_db();
        let zed = seed_entity(&db, "PERSON", "Zed", "hr");
        let alice = seed_entity(&db, "PERSON", "Alice", "hr");
        seed_entity(&db, "ORGANIZATION", "Acme", "it");

        let index = GraphIndex::from_db(&db, &GraphConfig::default()).unwrap();
        let g = index.ready().unwrap();

        let persons = g.nodes_by_type("PERSON");
        let ids: Vec<i64> = persons.iter().map(|i| g.node(*i).unwrap().id).collect();
        assert_eq!(ids, vec![zed, alice], "id order (Zed was inserted first)");
        assert_eq!(g.nodes_by_type("ORGANIZATION").len(), 1);
        assert!(g.nodes_by_type("GHOST").is_empty(), "unknown type → empty");
    }

    // Acceptance: an empty database yields an empty, VALID index (not an
    // error, not unavailable).
    #[test]
    fn empty_db_yields_empty_valid_index() {
        let db = in_memory_db();
        let index = GraphIndex::from_db(&db, &GraphConfig::default()).unwrap();
        assert!(index.is_available());
        let g = index.ready().unwrap();
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.edge_count(), 0);
        assert!(g.find_exact("anything", "any").is_none());
        assert!(g.nodes_by_type("PERSON").is_empty());
    }

    // Acceptance: repeated from_db over the same database yields an
    // equivalent index (node weights in node order, edge weights in edge
    // order).
    #[test]
    fn repeated_from_db_yields_equivalent_index() {
        let db = in_memory_db();
        let a = seed_entity(&db, "PERSON", "Alice", "hr");
        let b = seed_entity(&db, "PERSON", "Bob", "hr");
        let c = seed_entity(&db, "ORGANIZATION", "Acme", "it");
        seed_fact(&db, Some(a), "works_at", Some(c));
        seed_fact(&db, Some(b), "knows", Some(a));
        seed_link(&db, a, c, "same_entity");

        let first_index = GraphIndex::from_db(&db, &GraphConfig::default()).unwrap();
        let second_index = GraphIndex::from_db(&db, &GraphConfig::default()).unwrap();
        let first = first_index.ready().unwrap();
        let second = second_index.ready().unwrap();
        assert_eq!(snapshot(first), snapshot(second));
    }

    // Acceptance: flag semantics — enable_graph=false or load_on_startup=false
    // → the explicit unavailable state, not an error.
    #[test]
    fn disabled_flags_yield_unavailable_state() {
        let db = in_memory_db();
        let a = seed_entity(&db, "PERSON", "Alice", "hr");
        let b = seed_entity(&db, "PERSON", "Bob", "hr");
        seed_fact(&db, Some(a), "knows", Some(b));

        for config in [
            GraphConfig {
                enable_graph: false,
                ..Default::default()
            },
            GraphConfig {
                load_on_startup: false,
                ..Default::default()
            },
            GraphConfig {
                enable_graph: false,
                load_on_startup: false,
                ..Default::default()
            },
        ] {
            let index = GraphIndex::from_db(&db, &config).unwrap();
            assert!(
                matches!(index, GraphIndex::Unavailable),
                "config {config:?} must yield Unavailable"
            );
            assert!(!index.is_available());
            assert!(index.ready().is_none());
        }
    }

    // Facts with NULL endpoints (allowed by the v5 schema) must not become
    // edges: petgraph cannot attach an edge to a non-existent node (oracle
    // stored them against node id 0 and skipped them at traversal).
    #[test]
    fn facts_with_null_endpoints_are_skipped() {
        let db = in_memory_db();
        let a = seed_entity(&db, "PERSON", "Alice", "hr");
        seed_fact(&db, None, "located_in", Some(a));
        seed_fact(&db, Some(a), "reports_to", None);

        let index = GraphIndex::from_db(&db, &GraphConfig::default()).unwrap();
        let g = index.ready().unwrap();
        assert_eq!(g.node_count(), 1);
        assert_eq!(g.edge_count(), 0, "NULL endpoints must not become edges");
    }

    // Name-key collision (two types, same name+domain): the smallest entity
    // id wins in the name index; both stay reachable via the type index.
    #[test]
    fn name_collision_smallest_id_wins() {
        let db = in_memory_db();
        let person = seed_entity(&db, "PERSON", "Alice", "hr");
        let org = seed_entity(&db, "ORGANIZATION", "Alice", "hr");
        assert!(person < org, "fixture: insertion order is id order");

        let index = GraphIndex::from_db(&db, &GraphConfig::default()).unwrap();
        let g = index.ready().unwrap();
        assert_eq!(
            g.find_exact("alice", "hr"),
            Some(g.node_index(person).unwrap()),
            "smallest id wins the name key"
        );
        assert_eq!(g.nodes_by_type("PERSON").len(), 1);
        assert_eq!(g.nodes_by_type("ORGANIZATION").len(), 1);
    }

    // Domains are normalized in the node weight and in the name index (the
    // D4 boundary rule compares normalized values).
    #[test]
    fn domain_is_normalized_in_node_and_index() {
        let db = in_memory_db();
        let id = seed_entity(&db, "PERSON", "Alice", "  HR  ");

        let index = GraphIndex::from_db(&db, &GraphConfig::default()).unwrap();
        let g = index.ready().unwrap();
        let node = g.node(g.node_index(id).unwrap()).unwrap();
        assert_eq!(node.domain, "hr", "node domain must be normalized");
        assert!(
            g.find_exact("alice", "hr").is_some(),
            "index key must use the normalized domain"
        );
    }
}
