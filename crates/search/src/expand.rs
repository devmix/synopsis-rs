//! Graph expansion: enriching search results with graph context (design D8).
//!
//! Oracle mapping: `../synopsis/internal/search/graph_expansion.go`,
//! re-architected per the migration principles (functional copy, not a code
//! copy).
//!
//! Flow: collect the unique result entity ids (first-seen order, skipping
//! ids absent from the graph) → one batched
//! [`db::FactDao::list_by_entity_ids`] (approved facts only) → per entity:
//! BFS both directions via [`Graph::traverse`] (`max_depth`/`max_nodes` from
//! [`GraphConfig`]) → serialize the edges and facts into
//! `metadata["related_entities"]` in the oracle's wire shape
//! (`entity_id`/`name`/`domain` + optional `edges[]` + optional `facts[]`).
//!
//! **Non-fatal by contract (D8):** any expansion failure (fact batch query,
//! traversal) is a warning via `eprintln!` (crate convention — no logger in
//! the frozen stack) and the results are returned WITHOUT `related_entities`.
//!
//! **Conscious deviations from the oracle:**
//! - The Go `atomic.Pointer` graph swap (`SetGraph`) is gone: the expander
//!   borrows an immutable `&Graph` — the CLI rebuilds the searcher after
//!   re-indexing instead (D8).
//! - No context cancellation: the synchronous [`Graph::traverse`] cannot be
//!   cancelled, so the oracle's per-entity "expansion cancelled" warning has
//!   no trigger; a traversal error still degrades to that entity carrying no
//!   edges (defensive, mirrors the oracle).
//! - `expand` returns `()` instead of `(results, error)`: the oracle's error
//!   return is always `nil` (non-fatal by contract); a fallible signature
//!   would force every caller to write a no-op error arm.
//! - Each unique entity is serialized ONCE; the `serde_json::Value` is
//!   cloned per result reference (the oracle re-serialized per result).
//! - Facts with a `NULL` endpoint serialize the endpoint as `0` (the
//!   oracle's Go `int` zero value), preserving the wire shape.

use std::collections::{HashMap, HashSet};

use config::preset::GraphConfig;
use db::FactDao;
use graph::{EntityNode, Graph, TraversalEdge, TraverseOptions};
use serde_json::{Map, Value};

use crate::SearchResult;

/// Expands search-result entities with their graph context (design D8).
///
/// One instance per unit of work: borrows the long-lived [`Graph`] index
/// (injected by the caller — the expander exists only when the graph
/// config enables expansion AND an index is available) and a
/// connection-bound [`FactDao`] for the batch fact lookup.
pub struct GraphExpander<'conn> {
    graph: &'conn Graph,
    facts: &'conn FactDao<'conn>,
    options: TraverseOptions,
}

impl<'conn> GraphExpander<'conn> {
    /// Bind the expander to its collaborators. `max_depth`/`max_nodes` come
    /// from `config` (`0` → the traverser's defaults; an over-max depth is
    /// clamped by [`TraverseOptions::normalized`]). Direction is always
    /// Both (the oracle's `BFSOptions{Direction: DirectionBoth}`) and
    /// `follow_entity_links` stays `false`, matching the oracle: expansion
    /// never crosses domain boundaries.
    pub fn new(graph: &'conn Graph, facts: &'conn FactDao<'conn>, config: &GraphConfig) -> Self {
        Self {
            graph,
            facts,
            options: TraverseOptions {
                max_depth: config.max_depth.max(0) as u32,
                max_nodes: config.max_nodes.max(0) as usize,
                ..Default::default()
            },
        }
    }

    /// Expand every result in place with `metadata["related_entities"]`.
    ///
    /// Non-fatal by contract: any failure is a warning on stderr and the
    /// results are returned without graph context.
    pub fn expand(&self, results: &mut [SearchResult]) {
        match self.collect_expansions(results) {
            Ok(expansions) => attach_related_entities(results, &expansions),
            Err(err) => eprintln!(
                "graph expansion failed: {err}; returning results without related_entities"
            ),
        }
    }

    /// The serialized expansion of every unique result entity present in
    /// the graph (entity id → one `related_entities` entry).
    ///
    /// Entity ids are de-duplicated across the pool in first-seen order
    /// (one BFS + one facts lookup per entity — the oracle's `visited`
    /// set); id `0` (impossible by schema) and ids missing from the index
    /// are skipped.
    fn collect_expansions(&self, results: &[SearchResult]) -> Result<HashMap<i64, Value>, String> {
        let mut ids: Vec<i64> = Vec::new();
        let mut seen: HashSet<i64> = HashSet::new();
        for result in results.iter() {
            for entity in result.entities.iter() {
                let id = entity.id;
                if id == 0 || !seen.insert(id) || self.graph.node_index(id).is_none() {
                    continue;
                }
                ids.push(id);
            }
        }
        if ids.is_empty() {
            return Ok(HashMap::new());
        }

        // One batched query for every entity (approved facts only).
        let facts_by_entity = self
            .facts
            .list_by_entity_ids(&ids)
            .map_err(|err| err.to_string())?;

        let mut expansions: HashMap<i64, Value> = HashMap::with_capacity(ids.len());
        for &id in &ids {
            let Some(node) = self
                .graph
                .node_index(id)
                .and_then(|index| self.graph.node(index))
            else {
                // Cannot occur (membership checked above); keep the loop
                // total.
                continue;
            };
            let edges = match self.graph.traverse(id, &self.options) {
                Ok(result) => result.edges,
                Err(err) => {
                    // Non-fatal per entity (the oracle's "expansion
                    // cancelled" path): the entity keeps its node data
                    // without edges.
                    eprintln!("graph expansion: BFS from entity {id} failed: {err}");
                    Vec::new()
                }
            };
            let facts: &[db::Fact] = facts_by_entity.get(&id).map_or(&[], Vec::as_slice);
            expansions.insert(id, serialize_entity(node, &edges, facts));
        }
        Ok(expansions)
    }
}

/// Attach the serialized expansions to each result's
/// `metadata["related_entities"]`: one entry per result entity present in
/// the graph, in result-entity order (oracle `Expand`).
fn attach_related_entities(results: &mut [SearchResult], expansions: &HashMap<i64, Value>) {
    if expansions.is_empty() {
        return;
    }
    for result in results.iter_mut() {
        if result.entities.is_empty() {
            continue;
        }
        let related: Vec<Value> = result
            .entities
            .iter()
            .filter_map(|entity| expansions.get(&entity.id).cloned())
            .collect();
        if !related.is_empty() {
            result
                .metadata
                .insert("related_entities".to_owned(), Value::Array(related));
        }
    }
}

/// One `related_entities` entry: the graph node identity plus the BFS
/// edges and the entity's approved facts (oracle `entityData`). The
/// `edges`/`facts` keys are absent when empty (oracle contract).
fn serialize_entity(node: &EntityNode, edges: &[TraversalEdge], facts: &[db::Fact]) -> Value {
    let mut data = Map::new();
    data.insert("entity_id".to_owned(), node.id.into());
    data.insert("name".to_owned(), node.name.clone().into());
    data.insert("domain".to_owned(), node.domain.clone().into());
    let edges = serialize_edges(edges);
    if !edges.is_empty() {
        data.insert("edges".to_owned(), Value::Array(edges));
    }
    let facts = serialize_facts(facts);
    if !facts.is_empty() {
        data.insert("facts".to_owned(), Value::Array(facts));
    }
    Value::Object(data)
}

/// BFS edges in the oracle's wire shape (oracle `serializeEdges`):
/// `source_id`/`target_id`/`relation_type` always; `method` (when
/// non-empty), `confidence` (when > 0) and `evidence` (when present)
/// only when they carry a value.
fn serialize_edges(edges: &[TraversalEdge]) -> Vec<Value> {
    edges
        .iter()
        .map(|edge| {
            let mut data = Map::new();
            data.insert("source_id".to_owned(), edge.source.into());
            data.insert("target_id".to_owned(), edge.target.into());
            data.insert(
                "relation_type".to_owned(),
                edge.relation_type.clone().into(),
            );
            if let Some(method) = edge.method.as_deref().filter(|method| !method.is_empty()) {
                data.insert("method".to_owned(), method.into());
            }
            if let Some(confidence) = edge.confidence.filter(|confidence| *confidence > 0.0) {
                data.insert("confidence".to_owned(), confidence.into());
            }
            if let Some(evidence) = &edge.evidence {
                data.insert("evidence".to_owned(), evidence.clone().into());
            }
            Value::Object(data)
        })
        .collect()
}

/// Approved facts in the oracle's wire shape (oracle `serializeFacts`):
/// `id`/`predicate`/`subject_entity_id`/`object_entity_id` always (a
/// `NULL` endpoint is `0`, the oracle's Go `int` zero value), `metadata`
/// only when present (the raw JSON string, NOT parsed).
fn serialize_facts(facts: &[db::Fact]) -> Vec<Value> {
    facts
        .iter()
        .map(|fact| {
            let mut data = Map::new();
            data.insert("id".to_owned(), fact.id.into());
            data.insert("predicate".to_owned(), fact.predicate.clone().into());
            data.insert(
                "subject_entity_id".to_owned(),
                fact.subject_entity_id.unwrap_or(0).into(),
            );
            data.insert(
                "object_entity_id".to_owned(),
                fact.object_entity_id.unwrap_or(0).into(),
            );
            if let Some(metadata) = &fact.metadata_json {
                data.insert("metadata".to_owned(), metadata.clone().into());
            }
            Value::Object(data)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use config::preset::GraphConfig;
    use db::test_util::in_memory_db;
    use db::{ConnectionOrTx, Db, Entity, EntityDao, Fact, FactDao};
    use graph::EdgeKind;

    use super::*;
    use crate::SourceType;

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

    fn graph_of(entities: Vec<Entity>, facts: Vec<Fact>) -> Graph {
        Graph::from_rows(entities, facts, Vec::new())
    }

    fn result(chunk_id: i64, entity_ids: Vec<i64>) -> SearchResult {
        SearchResult {
            chunk_id,
            chunk_text: format!("chunk {chunk_id}"),
            document_id: 1,
            sequence_num: 0,
            start_offset: None,
            end_offset: None,
            document_path: String::new(),
            score: 1.0,
            rank: 1,
            source_type: SourceType::Lexical.as_str().to_owned(),
            metadata: Map::new(),
            entities: entity_ids
                .into_iter()
                .map(|id| entity(id, "PERSON", &format!("E{id}"), "hr"))
                .collect(),
        }
    }

    fn config(max_depth: i32, max_nodes: i32) -> GraphConfig {
        GraphConfig {
            max_depth,
            max_nodes,
            ..Default::default()
        }
    }

    /// Run `f` with an expander bound to a pooled connection.
    fn with_expander<T>(
        db: &Db,
        graph: &Graph,
        cfg: &GraphConfig,
        f: impl FnOnce(&GraphExpander<'_>) -> T,
    ) -> T {
        db.with_conn(|conn| {
            let facts = FactDao::new(ConnectionOrTx::Connection(conn));
            let expander = GraphExpander::new(graph, &facts, cfg);
            f(&expander)
        })
        .expect("connection checkout")
    }

    /// Seed an entity row in the database (satisfies the `facts` FK) and
    /// return its id.
    fn seed_entity(db: &Db, entity_type: &str, name: &str) -> i64 {
        db.exec_tx(|tx| {
            EntityDao::new(ConnectionOrTx::Transaction(&*tx)).create(
                entity_type,
                name,
                "hr",
                None,
                None,
                None,
            )
        })
        .expect("seed entity commits")
    }

    /// Seed an approved fact in the database (the batch lookup source) and
    /// return its id.
    fn seed_fact(
        db: &Db,
        subject: i64,
        predicate: &str,
        object: i64,
        metadata: Option<&str>,
    ) -> i64 {
        db.exec_tx(|tx| {
            FactDao::new(ConnectionOrTx::Transaction(&*tx)).create(
                Some(subject),
                predicate,
                Some(object),
                "",
                metadata,
                None,
                None,
            )
        })
        .expect("seed fact commits")
    }

    /// The `related_entities` array of `results[0]` (panics when absent).
    fn related(results: &[SearchResult]) -> Vec<Value> {
        results[0]
            .metadata
            .get("related_entities")
            .expect("related_entities present")
            .as_array()
            .expect("related_entities is an array")
            .clone()
    }

    // ── degenerate inputs ───────────────────────────────────────────────

    // Oracle TestGraphExpander_Expand_EmptyResults: an empty pool is a no-op.
    #[test]
    fn expand_empty_results_is_noop() {
        let db = in_memory_db();
        let graph = graph_of(vec![entity(1, "PERSON", "Alice", "hr")], Vec::new());
        let mut results: Vec<SearchResult> = Vec::new();
        with_expander(&db, &graph, &config(3, 100), |expander| {
            expander.expand(&mut results);
        });
        assert!(results.is_empty());
    }

    // Oracle TestGraphExpander_Expand_NoGraph / TestGraphExpansionDisabled:
    // entities absent from the graph (empty index) → no related_entities.
    #[test]
    fn expand_ignores_entities_absent_from_graph() {
        let db = in_memory_db();
        let graph = Graph::empty();
        let mut results = vec![result(1, vec![1, 2, 3])];
        with_expander(&db, &graph, &config(3, 100), |expander| {
            expander.expand(&mut results);
        });
        assert!(
            !results[0].metadata.contains_key("related_entities"),
            "no graph nodes → no related_entities"
        );
    }

    // A result without entities is skipped (oracle `len(Entities) == 0`).
    #[test]
    fn expand_skips_results_without_entities() {
        let db = in_memory_db();
        let graph = graph_of(
            vec![
                entity(1, "PERSON", "Alice", "hr"),
                entity(2, "PERSON", "Bob", "hr"),
            ],
            vec![fact(1, 1, "knows", 2)],
        );
        let mut results = vec![result(1, Vec::new())];
        with_expander(&db, &graph, &config(3, 100), |expander| {
            expander.expand(&mut results);
        });
        assert!(!results[0].metadata.contains_key("related_entities"));
    }

    // ── expansion content ───────────────────────────────────────────────

    // Oracle TestGraphExpander_MultipleEntities: every result entity present
    // in the graph gets an entry with the graph node's identity; the
    // connected entity carries its BFS edges.
    #[test]
    fn expand_multiple_entities() {
        let db = in_memory_db();
        let graph = graph_of(
            vec![
                entity(1, "PERSON", "Alice", "hr"),
                entity(2, "DEPARTMENT", "Engineering", "hr"),
                entity(3, "POLICY", "NDA", "hr"),
            ],
            vec![fact(1, 1, "works_in", 2), fact(2, 1, "owns", 3)],
        );
        let mut results = vec![result(1, vec![1, 2, 3])];
        with_expander(&db, &graph, &config(3, 100), |expander| {
            expander.expand(&mut results);
        });

        let list = related(&results);
        assert_eq!(list.len(), 3, "all three entities expanded");
        let by_id: HashMap<i64, &Value> = list
            .iter()
            .map(|entry| (entry["entity_id"].as_i64().expect("entity_id"), entry))
            .collect();
        assert_eq!(by_id[&1]["name"], "Alice");
        assert_eq!(by_id[&1]["domain"], "hr");
        assert_eq!(by_id[&2]["name"], "Engineering");
        assert_eq!(by_id[&3]["name"], "NDA");
        let alice_edges = by_id[&1].get("edges").expect("Alice has edges");
        assert!(
            !alice_edges
                .as_array()
                .expect("edges is an array")
                .is_empty(),
            "Alice is connected to Engineering and NDA"
        );
    }

    // The entry identity comes from the GRAPH NODE (normalized domain), not
    // from the result's entity row; an isolated node carries neither an
    // `edges` nor a `facts` key.
    #[test]
    fn expand_entry_identity_from_graph_node() {
        let db = in_memory_db();
        let graph = graph_of(vec![entity(1, "PERSON", "Alice", "  HR  ")], Vec::new());
        let mut results = vec![result(1, vec![1])];
        with_expander(&db, &graph, &config(3, 100), |expander| {
            expander.expand(&mut results);
        });
        let list = related(&results);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["entity_id"], 1);
        assert_eq!(list[0]["name"], "Alice");
        assert_eq!(list[0]["domain"], "hr", "the node's normalized domain");
        assert!(
            list[0].get("edges").is_none(),
            "isolated node → no edges key"
        );
        assert!(list[0].get("facts").is_none(), "no facts → no facts key");
    }

    // Oracle TestGraphExpander_Deduplication: an entity shared by two
    // results is expanded once and both results carry the entry.
    #[test]
    fn expand_deduplicates_shared_entities() {
        let db = in_memory_db();
        let graph = graph_of(
            vec![
                entity(1, "PERSON", "Alice", "hr"),
                entity(2, "DEPARTMENT", "Engineering", "hr"),
            ],
            vec![fact(1, 1, "works_in", 2)],
        );
        let mut results = vec![result(1, vec![1]), result(2, vec![1])];
        with_expander(&db, &graph, &config(3, 100), |expander| {
            expander.expand(&mut results);
        });
        for (i, r) in results.iter().enumerate() {
            let list = r
                .metadata
                .get("related_entities")
                .unwrap_or_else(|| panic!("result {i} must carry related_entities"))
                .as_array()
                .expect("an array");
            assert_eq!(list.len(), 1, "result {i}: one expanded entity");
            assert_eq!(list[0]["entity_id"], 1);
        }
    }

    // Oracle TestGraphExpander_DanglingEdge: an edge to a non-existent
    // entity never appears — the index builder skips dangling endpoints, so
    // the expansion carries no edges at all.
    #[test]
    fn expand_dangling_edge_excluded() {
        let db = in_memory_db();
        let graph = graph_of(
            vec![entity(1, "PERSON", "A", "hr")],
            vec![fact(1, 1, "relates_to", 9999)],
        );
        assert_eq!(graph.edge_count(), 0, "the index skips the dangling edge");
        let mut results = vec![result(1, vec![1])];
        with_expander(&db, &graph, &config(3, 100), |expander| {
            expander.expand(&mut results);
        });
        let list = related(&results);
        assert_eq!(list.len(), 1, "the entity itself is still expanded");
        assert!(list[0].get("edges").is_none(), "no dangling edges");
    }

    // ── BFS limits (GraphConfig) ────────────────────────────────────────

    // BFS depth limit from GraphConfig: a chain A→B→C→D stops at the
    // configured depth (center + max_depth hops).
    #[test]
    fn expand_respects_max_depth() {
        let db = in_memory_db();
        let graph = graph_of(
            (1..=4)
                .map(|id| entity(id, "PERSON", &format!("N{id}"), "hr"))
                .collect(),
            vec![
                fact(1, 1, "next", 2),
                fact(2, 2, "next", 3),
                fact(3, 3, "next", 4),
            ],
        );

        let edges_at = |max_depth: i32| -> Vec<(i64, i64)> {
            let mut results = vec![result(1, vec![1])];
            with_expander(&db, &graph, &config(max_depth, 1000), |expander| {
                expander.expand(&mut results);
            });
            let list = related(&results);
            list[0]
                .get("edges")
                .expect("edges present")
                .as_array()
                .expect("an array")
                .iter()
                .map(|e| {
                    (
                        e["source_id"].as_i64().expect("source_id"),
                        e["target_id"].as_i64().expect("target_id"),
                    )
                })
                .collect()
        };

        assert_eq!(edges_at(1), vec![(1, 2)]);
        assert_eq!(edges_at(2), vec![(1, 2), (2, 3)]);
        assert_eq!(edges_at(3), vec![(1, 2), (2, 3), (3, 4)]);
        // A depth beyond the chain length: the whole chain, no more.
        assert_eq!(edges_at(10), vec![(1, 2), (2, 3), (3, 4)]);
    }

    // Oracle TestGraphExpander_MaxNodesLimit: a star with 200 neighbors and
    // max_nodes=10 caps the result (max_nodes counts the center too).
    #[test]
    fn expand_respects_max_nodes() {
        let db = in_memory_db();
        const NEIGHBORS: i64 = 200;
        let entities = std::iter::once(entity(1, "PERSON", "Center", "hr"))
            .chain((2..=1 + NEIGHBORS).map(|id| entity(id, "DEPARTMENT", &format!("N{id}"), "hr")))
            .collect();
        let facts = (2..=1 + NEIGHBORS)
            .enumerate()
            .map(|(i, id)| fact(i as i64 + 1, 1, "connected_to", id))
            .collect();
        let graph = graph_of(entities, facts);

        let mut results = vec![result(1, vec![1])];
        with_expander(&db, &graph, &config(5, 10), |expander| {
            expander.expand(&mut results);
        });

        let list = related(&results);
        let edges = list[0]
            .get("edges")
            .expect("edges present")
            .as_array()
            .expect("an array");
        assert!(
            edges.len() <= 10,
            "max_nodes limit violated: {} edges",
            edges.len()
        );
        assert_eq!(edges.len(), 9, "center + 9 neighbors fills the limit");
    }

    // ── facts ───────────────────────────────────────────────────────────

    // Oracle TestGraphExpander_serializeFacts(_WithMetadata): the fact wire
    // shape — id/predicate/endpoints always; metadata (raw JSON string)
    // only when present.
    #[test]
    fn expand_facts_wire_shape() {
        let db = in_memory_db();
        let graph = graph_of(
            vec![
                entity(1, "PERSON", "Alice", "hr"),
                entity(2, "DEPARTMENT", "Engineering", "hr"),
            ],
            vec![fact(1, 1, "works_in", 2), fact(2, 1, "owns", 2)],
        );
        seed_entity(&db, "PERSON", "Alice");
        seed_entity(&db, "DEPARTMENT", "Engineering");
        seed_fact(
            &db,
            1,
            "works_in",
            2,
            Some(r#"{"threshold_amount":100,"condition":"active"}"#),
        );
        seed_fact(&db, 1, "owns", 2, None);

        let mut results = vec![result(1, vec![1])];
        with_expander(&db, &graph, &config(3, 100), |expander| {
            expander.expand(&mut results);
        });

        let list = related(&results);
        let facts = list[0]
            .get("facts")
            .expect("facts present")
            .as_array()
            .expect("an array");
        assert_eq!(facts.len(), 2, "both approved facts attached");
        let with_meta = facts
            .iter()
            .find(|f| f["predicate"] == "works_in")
            .expect("works_in present");
        assert_eq!(with_meta["subject_entity_id"], 1);
        assert_eq!(with_meta["object_entity_id"], 2);
        assert_eq!(
            with_meta["metadata"],
            r#"{"threshold_amount":100,"condition":"active"}"#
        );
        let no_meta = facts
            .iter()
            .find(|f| f["predicate"] == "owns")
            .expect("owns present");
        assert!(
            no_meta.get("metadata").is_none(),
            "no metadata_json → no metadata key"
        );
    }

    // Oracle TestGraphExpander_ExcludesDraftFacts: only approved facts reach
    // related_entities (the DAO filters `status = 'approved'`).
    #[test]
    fn expand_excludes_non_approved_facts() {
        let db = in_memory_db();
        let graph = graph_of(
            vec![
                entity(1, "PERSON", "Alice", "hr"),
                entity(2, "DEPARTMENT", "Engineering", "hr"),
            ],
            vec![fact(1, 1, "works_in", 2), fact(2, 1, "owns", 2)],
        );
        seed_entity(&db, "PERSON", "Alice");
        seed_entity(&db, "DEPARTMENT", "Engineering");
        let draft = seed_fact(&db, 1, "owns", 2, None);
        db.exec_tx(|tx| {
            tx.execute("UPDATE facts SET status = 'draft' WHERE id = ?", [draft])
                .map_err(db::DbError::from)
        })
        .expect("draft status set");
        seed_fact(&db, 1, "works_in", 2, None);

        let mut results = vec![result(1, vec![1])];
        with_expander(&db, &graph, &config(3, 100), |expander| {
            expander.expand(&mut results);
        });

        let list = related(&results);
        let facts = list[0]
            .get("facts")
            .expect("facts present")
            .as_array()
            .expect("an array");
        assert_eq!(facts.len(), 1, "only the approved fact");
        assert_eq!(facts[0]["predicate"], "works_in");
    }

    // ── non-fatal failure ───────────────────────────────────────────────

    // Oracle TestGraphExpander_Expand_FactBatchErrorNonFatal: a fact batch
    // failure (here: the facts table dropped — the oracle closed the
    // database) is a warning: the results return intact, without
    // related_entities.
    #[test]
    fn expand_fact_batch_failure_is_non_fatal() {
        let db = in_memory_db();
        let graph = graph_of(
            vec![
                entity(1, "PERSON", "Alice", "hr"),
                entity(2, "DEPARTMENT", "Engineering", "hr"),
            ],
            vec![fact(1, 1, "works_in", 2)],
        );

        // Control: expansion works while the facts table is intact.
        let mut control = vec![result(1, vec![1])];
        with_expander(&db, &graph, &config(3, 100), |expander| {
            expander.expand(&mut control);
        });
        assert!(
            control[0].metadata.contains_key("related_entities"),
            "control: expansion works"
        );

        // Break the fact source.
        db.with_conn(|conn| conn.execute("DROP TABLE facts", []))
            .expect("connection checkout")
            .expect("drop facts table");

        let mut broken = vec![result(2, vec![1])];
        with_expander(&db, &graph, &config(3, 100), |expander| {
            expander.expand(&mut broken);
        });
        assert_eq!(
            broken[0].chunk_text, "chunk 2",
            "the original result is preserved"
        );
        assert!(
            !broken[0].metadata.contains_key("related_entities"),
            "a failed expansion returns results without graph context"
        );
    }

    // ── wire shape (unit) ───────────────────────────────────────────────

    // Oracle TestGraphExpander_serializeEdges: the edge wire shape —
    // source/target/relation always; method/confidence/evidence only with
    // values (empty method and zero confidence are dropped).
    #[test]
    fn serialize_edges_wire_shape() {
        let fact_edge = TraversalEdge {
            source: 1,
            target: 2,
            kind: EdgeKind::Fact,
            relation_type: "relates_to".into(),
            method: None,
            confidence: None,
            evidence: None,
        };
        let link_edge = TraversalEdge {
            source: 2,
            target: 3,
            kind: EdgeKind::EntityLink,
            relation_type: "part_of".into(),
            method: Some("rule".into()),
            confidence: Some(0.9),
            evidence: Some("rule: hr/Alice -> it/Globex".into()),
        };
        let zero_conf = TraversalEdge {
            source: 3,
            target: 4,
            kind: EdgeKind::EntityLink,
            relation_type: "related_to".into(),
            method: Some(String::new()),
            confidence: Some(0.0),
            evidence: None,
        };

        let serialized = serialize_edges(&[fact_edge, link_edge, zero_conf]);
        assert_eq!(serialized.len(), 3);

        let fact = &serialized[0];
        assert_eq!(fact["source_id"], 1);
        assert_eq!(fact["target_id"], 2);
        assert_eq!(fact["relation_type"], "relates_to");
        assert!(fact.get("method").is_none(), "fact edges carry no method");
        assert!(
            fact.get("confidence").is_none(),
            "fact edges carry no confidence"
        );
        assert!(
            fact.get("evidence").is_none(),
            "fact edges carry no evidence"
        );

        let link = &serialized[1];
        assert_eq!(link["method"], "rule");
        assert_eq!(link["confidence"], 0.9);
        assert_eq!(link["evidence"], "rule: hr/Alice -> it/Globex");

        let zero = &serialized[2];
        assert!(zero.get("method").is_none(), "empty method is dropped");
        assert!(
            zero.get("confidence").is_none(),
            "zero confidence is dropped"
        );
    }
}
