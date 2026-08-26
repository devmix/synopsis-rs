//! The `get_entity_relations` and `get_entity_links` tools: knowledge-graph
//! traversal (BFS) and cross-domain entity links with provenance (design
//! D2/D4).
//!
//! Oracle mapping: `../synopsis/internal/mcp/handlers/{get_entity_relations.go,
//! get_entity_links.go}` + the shared `entity_resolve.go` (the id/name XOR
//! resolution is the same one `dossier.rs` ports).
//!
//! Thin handlers per design D2: parse the frozen-schema arguments → resolve
//! the entity (by `entity_id` XOR `entity_name`) → graph traverser / db DAOs
//! → oracle-shaped JSON. No business logic lives here.
//!
//! **Recorded deviations (house conventions, as in `dossier.rs`):**
//! 1. *Error text:* the oracle prefixes tool errors with `"Error …"` and says
//!    e.g. `"Entity Predicate %d not found"`; this crate uses the [`McpError`]
//!    conventions (e.g. `"entity with id N not found"`).
//! 2. *Unavailable graph:* the oracle's nil-graph check returns a tool error
//!    BEFORE argument parsing (a disabled graph is a missing resource, so the
//!    house [`McpError::NotFound`] carries it); the same check order is kept.
//! 3. *Edge `domain` field:* the oracle's `EdgeOut.Domain` is a dead field —
//!    the Go traverser never populates it, so its `omitempty` never emits it
//!    on the wire. The field is omitted here (wire-identical output, YAGNI).
//! 4. *No BFS timeout:* the oracle wraps the traversal in a 5 s context
//!    timeout (a Go idiom); the Rust traverser is synchronous.
//! 5. *Entity wire shape:* the oracle's `EntityNodeOut` has `domain,omitempty`;
//!    the shared [`EntityBrief`] (house convention) always emits `domain`. In
//!    practice identical: the v5 column is `NOT NULL`.
//! 6. *Nil-target guard:* the v5 schema enforces the link endpoint FKs
//!    (`foreign_keys=ON` on every pooled connection), so a dangling link row
//!    cannot exist via the DAOs; the guard (skip a missing target) is kept as
//!    defense in depth and is testable only by inserting past the FK.
//! 7. *Entity resolution:* the oracle's `ResolveEntity` is ported privately
//!    here as in `dossier.rs`; hoisting the shared resolver into
//!    `tools/entity` is a follow-up refactor outside this task's scope.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use db::{ConnectionOrTx, Entity, EntityDao, EntityLinkDao};
use graph::{EntityNode, GraphIndex, TraverseOptions};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::error::McpError;
use crate::tools::entity::{EntityBrief, entity_brief, node_brief};

/// The frozen tool name (`mcp-contract`).
pub const GET_ENTITY_RELATIONS: &str = "get_entity_relations";
/// The frozen tool name (`mcp-contract`).
pub const GET_ENTITY_LINKS: &str = "get_entity_links";

/// The default BFS depth (frozen schema: range 1-10, default 2).
const DEFAULT_DEPTH: i64 = 2;
/// The BFS depth floor (frozen schema: range 1-10).
const MIN_DEPTH: i64 = 1;
/// The BFS depth ceiling (frozen schema: range 1-10).
const MAX_DEPTH: i64 = 10;

// ── get_entity_relations wire shapes (oracle field order) ────────────────────

/// Provenance of a cross-domain edge (oracle `EdgeOut.Metadata`): emitted only
/// for entity-link edges when `include_cross_domain` is set (oracle condition
/// `includeCrossDomain && edge.Method != ""` — fact edges have no method).
#[derive(Debug, Serialize)]
struct EdgeMetadata {
    /// Entity-link method (`'rule'`/`'equals'`/`'llm'`).
    method: String,
    /// Entity-link confidence.
    confidence: f64,
    /// Entity-link evidence; absent when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence: Option<String>,
}

/// One traversed edge (oracle `EdgeOut`; the oracle's dead `domain` field is
/// omitted — recorded deviation 3).
#[derive(Debug, Serialize)]
struct EdgeOut {
    /// Source entity row id.
    source_id: i64,
    /// Target entity row id.
    target_id: i64,
    /// Source entity name (resolved from the traversal nodes).
    source_name: String,
    /// Target entity name (resolved from the traversal nodes).
    target_name: String,
    /// Fact predicate or link relation type.
    relation_type: String,
    /// Cross-domain provenance; absent unless `include_cross_domain` is set
    /// and the edge is an entity link.
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<EdgeMetadata>,
}

/// The `get_entity_relations` response (oracle `EntityRelationsResponse`):
/// field order matches the Go struct. `nodes`/`edges` are always present (no
/// omitempty in the oracle), possibly empty.
#[derive(Debug, Serialize)]
struct RelationsResponse {
    /// The center (start) entity, from the graph index.
    center_entity: EntityBrief,
    /// Connected nodes (the center excluded), in traversal order.
    nodes: Vec<EntityBrief>,
    /// The edges discovered, in traversal order.
    edges: Vec<EdgeOut>,
    /// `nodes.len()` — the center excluded from the count, as in the oracle.
    total_nodes: usize,
    /// `edges.len()`.
    total_edges: usize,
    /// The clamped depth actually used.
    traversal_depth: u32,
    /// BFS wall time in whole milliseconds.
    traversal_time_ms: u64,
}

// ── get_entity_links wire shapes (oracle field order) ────────────────────────

/// One cross-domain link with provenance (oracle `EntityLinkOut`).
#[derive(Debug, Serialize)]
struct LinkOut {
    /// The target entity row id.
    target_entity_id: i64,
    /// The target entity name.
    target_name: String,
    /// The target entity domain.
    target_domain: String,
    /// The link relation type.
    relation_type: String,
    /// Entity-link method (`'rule'`/`'equals'`/`'llm'`).
    method: String,
    /// Entity-link confidence.
    confidence: f64,
    /// Entity-link evidence; absent when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence: Option<String>,
}

/// The `get_entity_links` response (oracle `EntityLinksResponse`): `links` is
/// always present (no omitempty in the oracle), possibly empty.
#[derive(Debug, Serialize)]
struct LinksResponse {
    /// The resolved entity.
    entity: EntityBrief,
    /// The entity's cross-domain links, deduplicated by (target, relation).
    links: Vec<LinkOut>,
}

// ── arguments ────────────────────────────────────────────────────────────────

/// Parsed `get_entity_relations` arguments (frozen schema).
#[derive(Debug, Deserialize)]
struct RelationsArgs {
    /// Entity id (integer as string); XOR with `entity_name`.
    entity_id: Option<String>,
    /// Entity name (case-insensitive exact match); XOR with `entity_id`.
    entity_name: Option<String>,
    /// Domain used to disambiguate the name lookup.
    domain: Option<String>,
    /// BFS depth (number, range 1-10, default 2).
    depth: Option<Value>,
    /// Follow cross-domain entity links (default false).
    include_cross_domain: Option<bool>,
}

/// Parsed `get_entity_links` arguments (frozen schema).
#[derive(Debug, Deserialize)]
struct LinksArgs {
    /// Entity id (integer as string); XOR with `entity_name`.
    entity_id: Option<String>,
    /// Entity name (case-insensitive exact match); XOR with `entity_id`.
    entity_name: Option<String>,
    /// Domain used to disambiguate the name lookup.
    domain: Option<String>,
}

// ── shared helpers ───────────────────────────────────────────────────────────

/// Deserialize the argument object; `None` (no arguments) is the empty object,
/// so every filter is absent rather than a parse failure. Same convention as
/// the sibling tools.
fn deserialize_args<T: DeserializeOwned>(
    args: Option<&Value>,
    tool: &'static str,
) -> Result<T, McpError> {
    let value = args
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));
    serde_json::from_value(value).map_err(|err| McpError::InvalidArguments {
        tool,
        reason: err.to_string(),
    })
}

/// Serialize the response payload; a failure here is unreachable (all fields
/// are primitives), but the no-panic rule (design D7) keeps an error path.
fn to_value<T: Serialize>(tool: &'static str, response: T) -> Result<Value, McpError> {
    serde_json::to_value(response).map_err(|err| McpError::InvalidArguments {
        tool,
        reason: format!("serializing the response failed: {err}"),
    })
}

/// Parse `depth` (frozen schema: number, range 1-10, default 2). Lenient: a
/// number or a numeric string is accepted; anything else falls back to the
/// default (the oracle's `GetInt` leniency). Out-of-range values are clamped.
fn parse_depth(value: Option<&Value>) -> u32 {
    let raw = value.and_then(|value| match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|f| f as i64)),
        Value::String(text) => text.parse::<i64>().ok(),
        _ => None,
    });
    raw.unwrap_or(DEFAULT_DEPTH).clamp(MIN_DEPTH, MAX_DEPTH) as u32
}

/// Resolve an entity from `entity_id` XOR `entity_name` (oracle
/// `ResolveEntity`, with `entityType = ""`); `domain` disambiguates the name
/// lookup. A missing/ambiguous entity is a tool error, as in the oracle. Same
/// semantics as `dossier.rs`'s private resolver (recorded deviation 7).
fn resolve_entity(
    db: &db::Db,
    id_str: &str,
    name: &str,
    domain: &str,
    tool: &'static str,
) -> Result<Entity, McpError> {
    let has_id = !id_str.is_empty();
    let has_name = !name.is_empty();
    if !has_id && !has_name {
        return Err(McpError::InvalidArguments {
            tool,
            reason: "either 'entity_id' or 'entity_name' must be provided".to_owned(),
        });
    }
    if has_id && has_name {
        return Err(McpError::InvalidArguments {
            tool,
            reason: "provide either 'entity_id' or 'entity_name', not both".to_owned(),
        });
    }

    if has_id {
        let entity_id: i64 = id_str.parse().map_err(|_| McpError::InvalidArguments {
            tool,
            reason: format!("'entity_id' must be an integer, got {id_str:?}"),
        })?;
        let entity = db
            .with_conn(|conn| EntityDao::new(ConnectionOrTx::Connection(conn)).get_by_id(entity_id))
            .and_then(|result| result)?;
        return entity.ok_or_else(|| McpError::NotFound {
            what: format!("entity with id {entity_id} not found"),
        });
    }

    // Resolve by name. A domain filter narrows the lookup to one entity.
    if !domain.is_empty() {
        let entity = db
            .with_conn(|conn| {
                EntityDao::new(ConnectionOrTx::Connection(conn)).get_by_name_fold(name, domain)
            })
            .and_then(|result| result)?;
        return entity.ok_or_else(|| McpError::NotFound {
            what: format!("entity {name:?} not found in domain {domain:?}"),
        });
    }

    // No domain filter: a unique match succeeds; multiple matches list the
    // candidates so the caller can disambiguate.
    let mut matches = db
        .with_conn(|conn| EntityDao::new(ConnectionOrTx::Connection(conn)).list_by_name_fold(name))
        .and_then(|result| result)?;
    match matches.len() {
        0 => Err(McpError::NotFound {
            what: format!("entity {name:?} not found"),
        }),
        1 => Ok(matches.swap_remove(0)),
        _ => Err(McpError::InvalidArguments {
            tool,
            reason: format!(
                "multiple entities match {name:?}; specify one (optionally with a domain):\n{}",
                matches
                    .iter()
                    .map(|e| format!(
                        "- {} (id={}, type={}) [{}]",
                        e.name, e.id, e.entity_type, e.domain
                    ))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        }),
    }
}

// ── handlers ─────────────────────────────────────────────────────────────────

/// Handle the `get_entity_relations` tool call (design D2/D4).
///
/// `db` resolves the entity, `graph` is the injected index (Ready/Unavailable
/// per config); `args` is the raw JSON argument object (`None` = no
/// arguments). The result is the oracle-shaped payload the server serializes
/// into the tool response's text block.
pub fn handle_get_entity_relations(
    db: &db::Db,
    graph: &GraphIndex,
    args: Option<&Value>,
) -> Result<Value, McpError> {
    // Oracle parity: the unavailable graph is a tool error BEFORE argument
    // parsing (recorded deviation 2).
    let graph = graph.ready().ok_or_else(|| McpError::NotFound {
        what: "knowledge graph is not available (disabled or not loaded yet)".to_owned(),
    })?;

    let args: RelationsArgs = deserialize_args(args, GET_ENTITY_RELATIONS)?;
    let entity = resolve_entity(
        db,
        args.entity_id.as_deref().unwrap_or_default(),
        args.entity_name.as_deref().unwrap_or_default(),
        args.domain.as_deref().unwrap_or_default(),
        GET_ENTITY_RELATIONS,
    )?;
    let depth = parse_depth(args.depth.as_ref());
    let include_cross_domain = args.include_cross_domain.unwrap_or(false);

    // The resolved entity must be in the index (oracle `g.GetNode`); the
    // index can lag the database (built at startup, rebuilt on demand).
    if graph.node_index(entity.id).is_none() {
        return Err(McpError::NotFound {
            what: format!("entity with id {} not found in graph", entity.id),
        });
    }

    // BFS with the clamped frozen-schema depth; `max_nodes` 0 normalizes to
    // the oracle's 1000-node default.
    let options = TraverseOptions {
        max_depth: depth,
        follow_entity_links: include_cross_domain,
        ..Default::default()
    };
    let start = Instant::now();
    let result = graph.traverse(entity.id, &options)?;
    let traversal_time_ms = start.elapsed().as_millis() as u64;

    // Edge endpoint names resolve from the traversal nodes (every endpoint is
    // a visited node); the "" fallback is unreachable but keeps the no-panic
    // rule (design D7).
    let node_map: HashMap<i64, &EntityNode> =
        result.nodes.iter().map(|node| (node.id, node)).collect();
    let name_of = |id: i64| {
        node_map
            .get(&id)
            .map(|node| node.name.as_str())
            .unwrap_or_default()
    };

    let nodes = result
        .nodes
        .iter()
        .filter(|node| node.id != entity.id)
        .map(node_brief)
        .collect::<Vec<_>>();
    let edges = result
        .edges
        .iter()
        .map(|edge| {
            let metadata = if include_cross_domain {
                edge.method.as_ref().map(|method| EdgeMetadata {
                    method: method.clone(),
                    confidence: edge.confidence.unwrap_or(0.0),
                    evidence: edge.evidence.clone(),
                })
            } else {
                None
            };
            EdgeOut {
                source_id: edge.source,
                target_id: edge.target,
                source_name: name_of(edge.source).to_owned(),
                target_name: name_of(edge.target).to_owned(),
                relation_type: edge.relation_type.clone(),
                metadata,
            }
        })
        .collect::<Vec<_>>();

    let response = RelationsResponse {
        center_entity: node_brief(&result.center),
        nodes,
        edges,
        total_nodes: result.nodes.len() - 1,
        total_edges: result.edges.len(),
        traversal_depth: depth,
        traversal_time_ms,
    };
    to_value(GET_ENTITY_RELATIONS, response)
}

/// Handle the `get_entity_links` tool call (design D2/D4).
///
/// `db` is the injected database handle; `args` is the raw JSON argument
/// object (`None` = no arguments). The result is the oracle-shaped payload
/// the server serializes into the tool response's text block.
pub fn handle_get_entity_links(db: &db::Db, args: Option<&Value>) -> Result<Value, McpError> {
    let args: LinksArgs = deserialize_args(args, GET_ENTITY_LINKS)?;
    let entity = resolve_entity(
        db,
        args.entity_id.as_deref().unwrap_or_default(),
        args.entity_name.as_deref().unwrap_or_default(),
        args.domain.as_deref().unwrap_or_default(),
        GET_ENTITY_LINKS,
    )?;

    // The entity's links in both directions (the DAO's deterministic
    // (target, subject) order). Dedup by (target id, relation type) so a
    // bidirectional pair (A→B, B→A) yields one entry; first occurrence wins.
    let links = db
        .with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            let raw = EntityLinkDao::new(exec).list_by_entity(entity.id)?;
            let entities = EntityDao::new(exec);
            let mut seen: HashSet<(i64, String)> = HashSet::with_capacity(raw.len());
            let mut out = Vec::with_capacity(raw.len());
            for link in raw {
                // An incoming link (the entity is the stored target) resolves
                // the subject as the cross-domain target.
                let target_id = if link.target_entity_id == entity.id {
                    link.subject_entity_id
                } else {
                    link.target_entity_id
                };
                // Nil-target guard: skip links whose target row is gone
                // (recorded deviation 6).
                let Some(target) = entities.get_by_id(target_id)? else {
                    continue;
                };
                if !seen.insert((target.id, link.relation_type.clone())) {
                    continue;
                }
                out.push(LinkOut {
                    target_entity_id: target.id,
                    target_name: target.name.clone(),
                    target_domain: target.domain.clone(),
                    relation_type: link.relation_type.clone(),
                    method: link.method.clone(),
                    confidence: link.confidence,
                    evidence: link.evidence.clone(),
                });
            }
            Ok(out)
        })
        .and_then(|result| result)?;

    to_value(
        GET_ENTITY_LINKS,
        LinksResponse {
            entity: entity_brief(&entity),
            links,
        },
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use config::preset::GraphConfig;
    use db::{EntityLink, test_util};

    use super::*;

    /// Seed an in-memory KB in one transaction; returns the db and the value
    /// the seeder produced (usually the created ids). Same helper as
    /// `dossier.rs`.
    fn seed_db<T>(seed: impl FnOnce(ConnectionOrTx<'_>) -> Result<T, db::DbError>) -> (db::Db, T) {
        let db = test_util::in_memory_db();
        let value = db
            .exec_tx(|tx| seed(ConnectionOrTx::Transaction(&*tx)))
            .expect("seed transaction commits");
        (db, value)
    }

    /// A ready graph index over the db (config default: enabled + loaded).
    fn ready_graph(db: &db::Db) -> GraphIndex {
        GraphIndex::from_db(db, &GraphConfig::default()).expect("graph builds")
    }

    fn relations(db: &db::Db, graph: &GraphIndex, args: Option<Value>) -> Result<Value, McpError> {
        handle_get_entity_relations(db, graph, args.as_ref())
    }

    fn links(db: &db::Db, args: Option<Value>) -> Result<Value, McpError> {
        handle_get_entity_links(db, args.as_ref())
    }

    /// The oracle `setupTestGraph` fixture: Alice --works_in--> Engineering,
    /// Bob --works_in--> Engineering, Alice --owns--> NDA,
    /// Policy --requires--> Bob, Alice --reports_to--> Policy.
    fn seeded_relations_db() -> (db::Db, (i64, i64, i64, i64, i64)) {
        seed_db(|exec| {
            let entities = EntityDao::new(exec);
            let facts = db::FactDao::new(exec);
            let alice = entities.create("employee", "Alice", "", None, None, None)?;
            let engineering = entities.create("department", "Engineering", "", None, None, None)?;
            let nda = entities.create("policy", "NDA", "", None, None, None)?;
            let bob = entities.create("employee", "Bob", "", None, None, None)?;
            let policy = entities.create("policy", "Security Policy", "", None, None, None)?;
            facts.create(
                Some(alice),
                "works_in",
                Some(engineering),
                "",
                None,
                None,
                None,
            )?;
            facts.create(
                Some(bob),
                "works_in",
                Some(engineering),
                "",
                None,
                None,
                None,
            )?;
            facts.create(Some(alice), "owns", Some(nda), "", None, None, None)?;
            facts.create(Some(policy), "requires", Some(bob), "", None, None, None)?;
            facts.create(
                Some(alice),
                "reports_to",
                Some(policy),
                "",
                None,
                None,
                None,
            )?;
            Ok((alice, engineering, nda, bob, policy))
        })
    }

    // ── get_entity_relations ─────────────────────────────────────────────────

    /// Oracle `TestHandleGetEntityRelations_NilGraph` (the task's "no-graph
    /// degradation"): an unavailable graph is a tool error BEFORE argument
    /// parsing — `None` args must still yield the graph error, not an
    /// argument error.
    #[test]
    fn relations_unavailable_graph_errors_before_arg_parsing() {
        let db = test_util::in_memory_db();
        let graph = GraphIndex::Unavailable;

        let err = relations(&db, &graph, None).unwrap_err();
        assert!(matches!(err, McpError::NotFound { .. }), "got: {err:?}");
        assert!(
            err.to_string().contains("knowledge graph is not available"),
            "got: {err}"
        );
    }

    /// Oracle "empty entity_id and entity_name returns error" +
    /// `TestHandleGetEntityRelations_BothIDAndName`: the XOR is enforced.
    #[test]
    fn relations_requires_exactly_one_of_id_or_name() {
        let (db, _) = seeded_relations_db();
        let graph = ready_graph(&db);
        for args in [
            None,
            Some(serde_json::json!({})),
            Some(serde_json::json!({ "entity_id": "" })),
            Some(serde_json::json!({ "entity_id": "1", "entity_name": "Alice" })),
        ] {
            let err = relations(&db, &graph, args).unwrap_err();
            assert!(
                matches!(err, McpError::InvalidArguments { .. }),
                "got: {err:?}"
            );
        }
    }

    /// Oracle "non-integer entity_id returns error".
    #[test]
    fn relations_non_integer_id_is_an_error() {
        let (db, _) = seeded_relations_db();
        let graph = ready_graph(&db);
        let err = relations(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_id": "not_a_number" })),
        )
        .unwrap_err();
        assert!(
            matches!(err, McpError::InvalidArguments { .. }),
            "got: {err:?}"
        );
        assert!(err.to_string().contains("must be an integer"), "got: {err}");
    }

    /// Oracle "nonexistent entity returns error" + `EntityNameNotFound`.
    #[test]
    fn relations_missing_entity_is_not_found() {
        let (db, _) = seeded_relations_db();
        let graph = ready_graph(&db);
        for args in [
            serde_json::json!({ "entity_id": "99999" }),
            serde_json::json!({ "entity_name": "NonExistentEntity" }),
        ] {
            let err = relations(&db, &graph, Some(args)).unwrap_err();
            assert!(matches!(err, McpError::NotFound { .. }), "got: {err:?}");
            assert!(err.to_string().contains("not found"), "got: {err}");
        }
    }

    /// Oracle happy path + `_ResponseFields`: depth-1 traversal from Alice,
    /// response shape, and non-zero/non-empty edge endpoints.
    #[test]
    fn relations_depth_one_returns_direct_neighbors() {
        let (db, (alice, ..)) = seeded_relations_db();
        let graph = ready_graph(&db);

        let response = relations(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_id": alice.to_string(), "depth": 1 })),
        )
        .unwrap();

        assert_eq!(response["center_entity"]["id"], alice);
        assert_eq!(response["center_entity"]["name"], "Alice");
        assert_eq!(response["center_entity"]["type"], "employee");
        assert_eq!(response["total_nodes"], 3, "{response}");
        assert_eq!(response["total_edges"], 3, "{response}");
        assert_eq!(response["traversal_depth"], 1);
        assert!(response["traversal_time_ms"].is_u64(), "{response}");

        let names: Vec<&str> = response["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["Engineering", "NDA", "Security Policy"],
            "{response}"
        );

        // Oracle `_ResponseFields`: every edge carries non-zero ids and
        // non-empty endpoint names.
        for edge in response["edges"].as_array().unwrap() {
            assert_eq!(edge["source_id"].as_i64().unwrap(), alice);
            assert!(edge["target_id"].as_i64().unwrap() != 0, "{edge}");
            assert!(!edge["source_name"].as_str().unwrap().is_empty(), "{edge}");
            assert!(!edge["target_name"].as_str().unwrap().is_empty(), "{edge}");
        }
    }

    /// Depth-2 traversal: Bob is a second hop (reached through Engineering's
    /// incoming `works_in`).
    #[test]
    fn relations_depth_two_reaches_second_hop() {
        let (db, (alice, ..)) = seeded_relations_db();
        let graph = ready_graph(&db);

        let response = relations(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_id": alice.to_string(), "depth": 2 })),
        )
        .unwrap();
        assert_eq!(response["total_nodes"], 4, "{response}");
        assert_eq!(response["total_edges"], 4, "{response}");
        let names: Vec<&str> = response["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"Bob"), "Bob is a second hop: {response}");
    }

    /// Oracle "depth zero defaults to 1" + "depth over 10 capped at 10", plus
    /// the house lenient numeric string and the default for unparseable input.
    #[test]
    fn relations_depth_is_clamped_to_the_frozen_range() {
        let (db, alice) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            entities.create("employee", "Alice", "", None, None, None)
        });
        let graph = ready_graph(&db);
        for (depth, expected) in [
            (serde_json::json!(0), 1u32),
            (serde_json::json!(-3), 1u32),
            (serde_json::json!(50), 10u32),
            (serde_json::json!("7"), 7u32),
            (serde_json::json!("bogus"), 2u32),
        ] {
            let response = relations(
                &db,
                &graph,
                Some(serde_json::json!({ "entity_id": alice.to_string(), "depth": depth })),
            )
            .unwrap();
            assert_eq!(
                response["traversal_depth"].as_u64().unwrap(),
                expected as u64,
                "depth {depth}"
            );
        }
    }

    /// Oracle `TestHandleGetEntityRelations_DomainDisambiguation`: two
    /// same-named entities in different domains; a domain narrows the lookup
    /// (case-insensitive), no domain + multiple matches lists the candidates,
    /// and a single match succeeds.
    #[test]
    fn relations_domain_disambiguates_and_lists_candidates() {
        let (db, (id_hr, id_product, _acme, unique)) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            let facts = db::FactDao::new(exec);
            let id_hr = entities.create("employee", "Alice", "hr", None, None, None)?;
            let id_product = entities.create("employee", "Alice", "product", None, None, None)?;
            let acme = entities.create("organization", "Acme Corp", "hr", None, None, None)?;
            facts.create(Some(id_hr), "works_at", Some(acme), "hr", None, None, None)?;
            let unique = entities.create("employee", "UniqueBob", "hr", None, None, None)?;
            Ok((id_hr, id_product, acme, unique))
        });
        let graph = ready_graph(&db);

        let by_hr = relations(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_name": "Alice", "domain": "hr" })),
        )
        .unwrap();
        assert_eq!(by_hr["center_entity"]["id"], id_hr);

        let by_product = relations(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_name": "Alice", "domain": "PRODUCT" })),
        )
        .unwrap();
        assert_eq!(by_product["center_entity"]["id"], id_product);

        let ambiguous = relations(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_name": "Alice" })),
        )
        .unwrap_err();
        assert!(
            matches!(ambiguous, McpError::InvalidArguments { .. }),
            "got: {ambiguous:?}"
        );
        assert!(
            ambiguous.to_string().contains("multiple entities match"),
            "got: {ambiguous}"
        );

        let single = relations(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_name": "UniqueBob" })),
        )
        .unwrap();
        assert_eq!(single["center_entity"]["id"], unique);
    }

    /// The index can lag the database (built at startup, rebuilt on demand):
    /// an entity present in the db but absent from the index is a not-found
    /// tool error (oracle `g.GetNode` miss; recorded deviation 1 wording).
    #[test]
    fn relations_entity_missing_from_index_is_not_found() {
        let (db, _) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            entities.create("employee", "Alice", "", None, None, None)
        });
        let graph = ready_graph(&db);

        // Insert AFTER the index is built: the graph cannot know this entity.
        let late = db
            .with_conn(|conn| {
                EntityDao::new(ConnectionOrTx::Connection(conn)).create(
                    "employee",
                    "LateArriver",
                    "",
                    None,
                    None,
                    None,
                )
            })
            .and_then(|result| result)
            .expect("late insert");

        let err = relations(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_id": late.to_string() })),
        )
        .unwrap_err();
        assert!(matches!(err, McpError::NotFound { .. }), "got: {err:?}");
        assert!(err.to_string().contains("not found in graph"), "got: {err}");
    }

    /// `include_cross_domain`: with the flag, entity-link edges cross domains
    /// and carry provenance metadata; the cross-domain FACT edge stays blocked
    /// (D4); without the flag, links are not followed and no metadata is
    /// emitted.
    #[test]
    fn relations_cross_domain_flag_controls_links_and_metadata() {
        let (db, (alice, ..)) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            let facts = db::FactDao::new(exec);
            let links = EntityLinkDao::new(exec);
            let alice = entities.create("employee", "Alice", "hr", None, None, None)?;
            let bob = entities.create("employee", "Bob", "hr", None, None, None)?;
            let acme = entities.create("organization", "Acme", "it", None, None, None)?;
            let globex = entities.create("organization", "Globex", "it", None, None, None)?;
            facts.create(Some(alice), "knows", Some(bob), "hr", None, None, None)?;
            // A cross-domain FACT edge: blocked by D4 even with the flag.
            facts.create(Some(alice), "works_at", Some(acme), "hr", None, None, None)?;
            links.create(&EntityLink {
                subject_entity_id: alice,
                target_entity_id: globex,
                relation_type: "same_entity".into(),
                method: "rule".into(),
                confidence: 0.95,
                evidence: Some("rule: hr/Alice -> it/Globex".into()),
            })?;
            Ok((alice, bob, acme, globex))
        });
        let graph = ready_graph(&db);

        // include_cross_domain = true: Globex crosses via the LINK (metadata
        // present), Bob via the fact (no metadata), Acme stays blocked.
        let with = relations(
            &db,
            &graph,
            Some(serde_json::json!({
                "entity_id": alice.to_string(),
                "include_cross_domain": true,
            })),
        )
        .unwrap();
        let names: Vec<&str> = with["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"Globex"), "{with}");
        assert!(names.contains(&"Bob"), "{with}");
        assert!(
            !names.contains(&"Acme"),
            "cross-domain fact must stay blocked: {with}"
        );

        let globex_edge = with["edges"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["target_name"] == "Globex")
            .unwrap();
        assert_eq!(globex_edge["metadata"]["method"], "rule");
        assert_eq!(globex_edge["metadata"]["confidence"], 0.95);
        assert_eq!(
            globex_edge["metadata"]["evidence"],
            "rule: hr/Alice -> it/Globex"
        );
        let bob_edge = with["edges"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["target_name"] == "Bob")
            .unwrap();
        assert!(bob_edge.get("metadata").is_none(), "{with}");

        // include_cross_domain = false: no link followed, no metadata.
        let without = relations(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_id": alice.to_string() })),
        )
        .unwrap();
        let names: Vec<&str> = without["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["Bob"], "{without}");
        for edge in without["edges"].as_array().unwrap() {
            assert!(edge.get("metadata").is_none(), "{without}");
        }
    }

    // ── get_entity_links ─────────────────────────────────────────────────────

    /// Oracle `TestHandleGetEntityLinks` error cases: missing args, empty id,
    /// non-integer id, and a not-found id.
    #[test]
    fn links_argument_errors() {
        let (db, _) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            entities.create("employee", "Alice", "", None, None, None)
        });

        let err = links(&db, None).unwrap_err();
        assert!(
            matches!(err, McpError::InvalidArguments { .. }),
            "got: {err:?}"
        );

        let err = links(&db, Some(serde_json::json!({ "entity_id": "" }))).unwrap_err();
        assert!(
            matches!(err, McpError::InvalidArguments { .. }),
            "got: {err:?}"
        );

        let err = links(&db, Some(serde_json::json!({ "entity_id": "abc" }))).unwrap_err();
        assert!(
            matches!(err, McpError::InvalidArguments { .. }),
            "got: {err:?}"
        );
        assert!(err.to_string().contains("must be an integer"), "got: {err}");

        let err = links(&db, Some(serde_json::json!({ "entity_id": "99999" }))).unwrap_err();
        assert!(matches!(err, McpError::NotFound { .. }), "got: {err:?}");
    }

    /// Oracle `TestHandleGetEntityLinks_EntityWithNoLinks`: `links` is an
    /// empty array, still present in the response.
    #[test]
    fn links_empty_entity_returns_empty_links_array() {
        let (db, id) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            entities.create("employee", "SoloEmployee", "", None, None, None)
        });
        let response = links(
            &db,
            Some(serde_json::json!({ "entity_id": id.to_string() })),
        )
        .unwrap();
        assert_eq!(response["entity"]["id"], id);
        assert_eq!(response["entity"]["name"], "SoloEmployee");
        assert_eq!(response["links"], serde_json::json!([]), "{response}");
    }

    /// Oracle `TestHandleGetEntityLinks_EntityWithLinks`: the entity's links
    /// with the DAO's deterministic `(target, subject)` order and the full
    /// provenance payload.
    #[test]
    fn links_returns_entity_links_with_provenance() {
        let (db, (alice, engineering, nda)) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            let links = EntityLinkDao::new(exec);
            let alice = entities.create("employee", "Alice", "hr", None, None, None)?;
            let engineering =
                entities.create("department", "Engineering", "hr", None, None, None)?;
            let nda = entities.create("policy", "NDA", "hr", None, None, None)?;
            links.create(&EntityLink {
                subject_entity_id: alice,
                target_entity_id: engineering,
                relation_type: "located_in".into(),
                method: "rule".into(),
                confidence: 0.95,
                evidence: Some("Rule-based matching on keyword 'Alice'".into()),
            })?;
            links.create(&EntityLink {
                subject_entity_id: alice,
                target_entity_id: nda,
                relation_type: "owns".into(),
                method: "llm".into(),
                confidence: 0.78,
                evidence: Some("LLM inference from context".into()),
            })?;
            Ok((alice, engineering, nda))
        });

        let response = links(
            &db,
            Some(serde_json::json!({ "entity_id": alice.to_string() })),
        )
        .unwrap();
        assert_eq!(response["entity"]["id"], alice);

        let out = response["links"].as_array().unwrap();
        assert_eq!(out.len(), 2, "{response}");
        // DAO order: (target, subject) ascending → Engineering, then NDA.
        assert_eq!(out[0]["target_entity_id"], engineering);
        assert_eq!(out[0]["target_name"], "Engineering");
        assert_eq!(out[0]["target_domain"], "hr");
        assert_eq!(out[0]["relation_type"], "located_in");
        assert_eq!(out[0]["method"], "rule");
        assert_eq!(out[0]["confidence"], 0.95);
        assert_eq!(out[0]["evidence"], "Rule-based matching on keyword 'Alice'");
        assert_eq!(out[1]["target_entity_id"], nda);
        assert_eq!(out[1]["relation_type"], "owns");
        assert_eq!(out[1]["method"], "llm");
        assert_eq!(out[1]["confidence"], 0.78);
    }

    /// Oracle `TestHandleGetEntityLinks_LinksProvenance`: method / confidence
    /// / evidence round-trip; an absent evidence value is omitted from the
    /// wire (oracle `omitempty`).
    #[test]
    fn links_provenance_round_trips_and_omits_absent_evidence() {
        let (db, bob) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            let links = EntityLinkDao::new(exec);
            let bob = entities.create("employee", "Bob", "", None, None, None)?;
            let it = entities.create("department", "IT", "", None, None, None)?;
            links.create(&EntityLink {
                subject_entity_id: bob,
                target_entity_id: it,
                relation_type: "resides_in".into(),
                method: "llm".into(),
                confidence: 0.85,
                evidence: Some("Found in chapter 3, paragraph 2".into()),
            })?;
            Ok(bob)
        });

        let response = links(
            &db,
            Some(serde_json::json!({ "entity_id": bob.to_string() })),
        )
        .unwrap();
        let out = response["links"].as_array().unwrap();
        assert_eq!(out.len(), 1, "{response}");
        assert_eq!(out[0]["method"], "llm");
        assert_eq!(out[0]["confidence"], 0.85);
        assert!(
            out[0]["evidence"].as_str().unwrap().contains("chapter 3"),
            "{response}"
        );

        // A link without evidence omits the key.
        let (db2, carol) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            let links = EntityLinkDao::new(exec);
            let carol = entities.create("employee", "Carol", "", None, None, None)?;
            let ops = entities.create("department", "Ops", "", None, None, None)?;
            links.create(&EntityLink {
                subject_entity_id: carol,
                target_entity_id: ops,
                relation_type: "resides_in".into(),
                method: "rule".into(),
                confidence: 0.6,
                evidence: None,
            })?;
            Ok(carol)
        });
        let response2 = links(
            &db2,
            Some(serde_json::json!({ "entity_id": carol.to_string() })),
        )
        .unwrap();
        assert!(
            response2["links"][0].get("evidence").is_none(),
            "{response2}"
        );
    }

    /// Oracle `TestHandleGetEntityLinks_DedupBidirectional`: an A→B / B→A
    /// pair with the same relation type yields ONE entry; the first
    /// occurrence (DAO order) wins.
    #[test]
    fn links_dedup_bidirectional_pairs() {
        let (db, (alice, office)) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            let links = EntityLinkDao::new(exec);
            let alice = entities.create("employee", "Alice", "", None, None, None)?;
            let office = entities.create("department", "Office", "", None, None, None)?;
            links.create(&EntityLink {
                subject_entity_id: alice,
                target_entity_id: office,
                relation_type: "works_in".into(),
                method: "rule".into(),
                confidence: 1.0,
                evidence: None,
            })?;
            links.create(&EntityLink {
                subject_entity_id: office,
                target_entity_id: alice,
                relation_type: "works_in".into(),
                method: "rule".into(),
                confidence: 1.0,
                evidence: None,
            })?;
            Ok((alice, office))
        });

        let response = links(
            &db,
            Some(serde_json::json!({ "entity_id": alice.to_string() })),
        )
        .unwrap();
        let out = response["links"].as_array().unwrap();
        assert_eq!(out.len(), 1, "one entry per (target, relation): {response}");
        assert_eq!(out[0]["target_entity_id"], office);
        assert_eq!(out[0]["relation_type"], "works_in");
    }

    /// Oracle `TestHandleGetEntityLinks_NilTargetGuard`: a link whose target
    /// row is gone is skipped silently. The v5 FKs make such a row
    /// uncreatable through the DAOs (recorded deviation 6), so the fixture
    /// inserts past the FK on this connection only.
    #[test]
    fn links_skip_missing_target_rows() {
        let (db, charlie) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            entities.create("employee", "Charlie", "", None, None, None)
        });

        db.with_conn(|conn| {
            conn.pragma_update(None, "foreign_keys", "OFF")
                .and_then(|_| {
                    conn.execute(
                        "INSERT INTO entity_links \
                         (subject_entity_id, target_entity_id, relation_type, method, confidence, evidence) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        (charlie, 99_999, "knows", "rule", 1.0f64, None::<String>),
                    )
                })
                .and_then(|_| conn.pragma_update(None, "foreign_keys", "ON"))
                .map_err(db::DbError::from)
        })
        .and_then(|result| result)
        .expect("dangling link row inserted past the FK");

        let response = links(
            &db,
            Some(serde_json::json!({ "entity_id": charlie.to_string() })),
        )
        .unwrap();
        assert_eq!(response["links"], serde_json::json!([]), "{response}");
    }

    /// Oracle `TestHandleGetEntityLinks_DomainDisambiguation`: a domain
    /// narrows the lookup (case-insensitive), multiple matches without a
    /// domain are an error listing the candidates, and a single match
    /// succeeds.
    #[test]
    fn links_domain_disambiguates_and_lists_candidates() {
        let (db, (id_hr, id_product, unique)) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            let id_hr = entities.create("employee", "Alice", "hr", None, None, None)?;
            let id_product = entities.create("employee", "Alice", "product", None, None, None)?;
            let unique = entities.create("employee", "UniqueBob", "hr", None, None, None)?;
            Ok((id_hr, id_product, unique))
        });

        let by_hr = links(
            &db,
            Some(serde_json::json!({ "entity_name": "Alice", "domain": "hr" })),
        )
        .unwrap();
        assert_eq!(by_hr["entity"]["id"], id_hr);

        let by_product = links(
            &db,
            Some(serde_json::json!({ "entity_name": "Alice", "domain": "PRODUCT" })),
        )
        .unwrap();
        assert_eq!(by_product["entity"]["id"], id_product);

        let ambiguous =
            links(&db, Some(serde_json::json!({ "entity_name": "Alice" }))).unwrap_err();
        assert!(
            matches!(ambiguous, McpError::InvalidArguments { .. }),
            "got: {ambiguous:?}"
        );
        assert!(
            ambiguous.to_string().contains("multiple entities match"),
            "got: {ambiguous}"
        );

        let single = links(&db, Some(serde_json::json!({ "entity_name": "UniqueBob" }))).unwrap();
        assert_eq!(single["entity"]["id"], unique);
    }
}
