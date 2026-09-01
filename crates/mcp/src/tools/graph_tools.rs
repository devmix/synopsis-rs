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
//!    the shared `EntityBrief` (house convention) always emits `domain`. In
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
