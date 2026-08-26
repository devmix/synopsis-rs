//! The `get_entity_dossier` tool: a complete entity dossier — the entity, its
//! approved facts (with per-fact sources), its source documents, related
//! entities via graph BFS, and cross-domain links with provenance.
//!
//! Oracle mapping: `../synopsis/internal/mcp/handlers/{get_entity_dossier.go,
//! entity_resolve.go}`.
//!
//! Thin handler per design D2: parse the frozen-schema arguments → resolve the
//! entity (by `entity_id` XOR `entity_name`) → db DAOs + graph traverser →
//! oracle-shaped JSON. No business logic lives here.
//!
//! **Recorded deviations (house conventions, as in `facts.rs`):**
//! 1. *Error text:* the oracle prefixes tool errors with `"Error …"` and says
//!    `"Entity Predicate %d not found"`; this crate uses the [`McpError`]
//!    conventions (e.g. `"entity with id N not found"`).
//! 2. *Fact-source errors:* the oracle ignores `GetByFactID` errors
//!    (`if err == nil`); this crate propagates them as tool errors.
//! 3. *Direct-link list errors:* the oracle ignores `ListByEntity` errors; this
//!    crate propagates them.
//! 4. *No BFS timeout:* the oracle wraps the traversal in a 5 s context
//!    timeout (a Go idiom); the Rust traverser is synchronous (callers wrap it
//!    in `spawn_blocking` where cancellation is needed).
//! 5. *Cross-domain comparison:* the oracle compares the normalized
//!    graph-node domain against the RAW entity domain, and applies NO domain
//!    filter to the direct-links pass. Both quirks are replicated so the wire
//!    output matches the oracle's.
//! 6. *`confidence` 0.0:* the oracle's `omitempty` drops a zero confidence; the
//!    house convention (as in `entities_catalog`) keeps an explicit `0.0`. In
//!    practice the column is `NULL` when absent, so this is theoretical.

use std::collections::HashMap;

use db::{
    ConnectionOrTx, Document, DocumentDao, Entity, EntityDao, EntityLinkDao, EntitySourceDao, Fact,
    FactDao, FactSource, FactSourceDao,
};
use graph::{EntityNode, GraphIndex, TraverseOptions};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::error::McpError;
use crate::tools::entity::{EntityBrief, node_brief};
use crate::tools::facts::FactSourceInfo;

/// The frozen tool name (`mcp-contract`).
pub const GET_ENTITY_DOSSIER: &str = "get_entity_dossier";

/// The hard cap on facts returned per dossier (oracle `facts[:100]`).
const MAX_FACTS: usize = 100;
/// The BFS node cap (oracle `MaxNodes: 100`).
const BFS_MAX_NODES: usize = 100;
/// The default BFS depth (frozen schema: range 1-5, default 2).
const DEFAULT_DEPTH: i64 = 2;
/// The BFS depth floor (frozen schema: range 1-5).
const MIN_DEPTH: i64 = 1;
/// The BFS depth ceiling (frozen schema: range 1-5).
const MAX_DEPTH: i64 = 5;

// ── wire shapes (oracle field order) ────────────────────────────────────────

/// The dossier entity (oracle `DossierEntityInfo`): field order matches Go.
#[derive(Debug, Serialize)]
struct DossierEntity {
    /// Entity row id.
    id: i64,
    /// Entity name.
    name: String,
    /// Entity type.
    #[serde(rename = "type")]
    r#type: String,
    /// Entity domain (`''` = global).
    domain: String,
    /// Free-text description; absent when NULL or empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    /// Extraction confidence; absent when NULL.
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<f64>,
    /// The raw `metadata_json` string; absent when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<String>,
}

/// A dossier fact (oracle `DossierFact`): field order matches Go.
#[derive(Debug, Serialize)]
struct DossierFact {
    /// Fact row id.
    id: i64,
    /// The relation predicate.
    predicate: String,
    /// Subject entity id; absent when the fact has no subject.
    #[serde(skip_serializing_if = "Option::is_none")]
    subject_entity_id: Option<i64>,
    /// Object entity id; absent when the fact has no object.
    #[serde(skip_serializing_if = "Option::is_none")]
    object_entity_id: Option<i64>,
    /// Fact domain (`''` = global).
    domain: String,
    /// The raw `metadata` string; absent when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<String>,
    /// Fact status.
    status: String,
    /// Validity interval start, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_from: Option<String>,
    /// Validity interval end, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_to: Option<String>,
    /// Source-count weight.
    weight: i64,
    /// The fact's sources; absent when it has none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sources: Vec<FactSourceInfo>,
}

/// A source document (oracle `SourceDoc`): field order matches Go.
#[derive(Debug, Serialize)]
struct SourceDoc {
    /// Document row id.
    id: i64,
    /// Originating source type.
    source_type: String,
    /// Path of the original file.
    original_path: String,
    /// The raw `metadata_json` string; absent when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<String>,
}

/// A cross-domain link (oracle `CrossDomainLink`): field order matches Go.
#[derive(Debug, Clone, Serialize)]
struct CrossDomainLink {
    /// The target entity row id.
    target_entity_id: i64,
    /// The target entity name.
    target_name: String,
    /// The target entity domain.
    target_domain: String,
    /// The relation types observed for this target (deduplicated, encounter
    /// order); absent when there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    relation_types: Vec<String>,
    /// Entity-link method; absent when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<String>,
    /// Entity-link confidence; absent when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<f64>,
    /// Entity-link evidence; absent when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence: Option<String>,
}

/// The `get_entity_dossier` response (oracle `EntityDossierResponse`): field
/// order matches the Go struct; the empty sections are omitted (omitempty).
#[derive(Debug, Serialize)]
struct DossierResponse {
    /// The resolved entity.
    entity: DossierEntity,
    /// Approved facts; absent when excluded or there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    facts: Vec<DossierFact>,
    /// Source documents; absent when excluded or there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sources: Vec<SourceDoc>,
    /// Related entities via BFS; absent when the graph is unavailable or there
    /// are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    related_entities: Vec<EntityBrief>,
    /// Cross-domain links; absent when there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cross_domain_links: Vec<CrossDomainLink>,
}

// ── arguments ────────────────────────────────────────────────────────────────

/// Parsed `get_entity_dossier` arguments (frozen schema).
#[derive(Debug, Deserialize)]
struct DossierArgs {
    /// Entity id (integer as string); XOR with `entity_name`.
    entity_id: Option<String>,
    /// Entity name (case-insensitive exact match); XOR with `entity_id`.
    entity_name: Option<String>,
    /// Domain used to disambiguate the name lookup.
    domain: Option<String>,
    /// BFS depth (number, range 1-5, default 2).
    depth: Option<Value>,
    /// Include approved facts (default true).
    include_facts: Option<bool>,
    /// Include source documents (default true).
    include_sources: Option<bool>,
}

// ── shared helpers ───────────────────────────────────────────────────────────

/// Deserialize the argument object; `None` (no arguments) is the empty object,
/// so every filter is absent rather than a parse failure. Same convention as the
/// sibling tools.
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

/// Parse `depth` (frozen schema: number, range 1-5, default 2). Lenient: a
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

/// Resolve an entity from `entity_id` XOR `entity_name` (oracle `ResolveEntity`,
/// with `entityType = ""` for the dossier). `domain` disambiguates the name
/// lookup. A missing/ambiguous entity is a tool error, as in the oracle.
fn resolve_entity(db: &db::Db, args: &DossierArgs) -> Result<Entity, McpError> {
    let id_str = args.entity_id.as_deref().unwrap_or_default();
    let name = args.entity_name.as_deref().unwrap_or_default();
    let domain = args.domain.as_deref().unwrap_or_default();

    let has_id = !id_str.is_empty();
    let has_name = !name.is_empty();
    if !has_id && !has_name {
        return Err(McpError::InvalidArguments {
            tool: GET_ENTITY_DOSSIER,
            reason: "either 'entity_id' or 'entity_name' must be provided".to_owned(),
        });
    }
    if has_id && has_name {
        return Err(McpError::InvalidArguments {
            tool: GET_ENTITY_DOSSIER,
            reason: "provide either 'entity_id' or 'entity_name', not both".to_owned(),
        });
    }

    if has_id {
        let entity_id: i64 = id_str.parse().map_err(|_| McpError::InvalidArguments {
            tool: GET_ENTITY_DOSSIER,
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
            tool: GET_ENTITY_DOSSIER,
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

// ── wire mapping ─────────────────────────────────────────────────────────────

/// Map a stored entity to the wire `entity` object (oracle field mapping in
/// `get_entity_dossier.go`).
fn dossier_entity(entity: &Entity) -> DossierEntity {
    DossierEntity {
        id: entity.id,
        name: entity.name.clone(),
        r#type: entity.entity_type.clone(),
        domain: entity.domain.clone(),
        description: entity
            .description
            .as_deref()
            .filter(|description| !description.is_empty())
            .map(str::to_owned),
        confidence: entity.confidence,
        metadata: entity
            .metadata_json
            .as_deref()
            .filter(|metadata| !metadata.is_empty())
            .map(str::to_owned),
    }
}

/// Map a stored fact plus its sources to the wire fact object (oracle field
/// mapping in `get_entity_dossier.go`).
fn dossier_fact(fact: &Fact, fact_sources: &[FactSource]) -> DossierFact {
    DossierFact {
        id: fact.id,
        predicate: fact.predicate.clone(),
        subject_entity_id: fact.subject_entity_id,
        object_entity_id: fact.object_entity_id,
        domain: fact.domain.clone(),
        metadata: fact
            .metadata_json
            .as_deref()
            .filter(|metadata| !metadata.is_empty())
            .map(str::to_owned),
        status: fact.status.clone(),
        valid_from: fact.valid_from.clone(),
        valid_to: fact.valid_to.clone(),
        weight: fact.weight,
        sources: fact_sources
            .iter()
            .map(|source| FactSourceInfo {
                document_id: source.document_id,
                quote: source
                    .quote
                    .as_deref()
                    .filter(|quote| !quote.is_empty())
                    .map(str::to_owned),
                extracted_at: source.extracted_at.clone(),
            })
            .collect(),
    }
}

/// Map a stored document to the wire source object (oracle field mapping in
/// `get_entity_dossier.go`).
fn source_doc(doc: &Document) -> SourceDoc {
    SourceDoc {
        id: doc.id,
        source_type: doc.source_type.clone(),
        original_path: doc.original_path.clone(),
        metadata: doc
            .metadata_json
            .as_deref()
            .filter(|metadata| !metadata.is_empty())
            .map(str::to_owned),
    }
}

// ── cross-domain link merging ────────────────────────────────────────────────

/// Related entities (BFS nodes minus the center) plus the cross-domain links
/// deduplicated by target id. A traversal error (e.g. the entity is not in the
/// index) or an unavailable graph yields empty sections — the oracle's
/// `if err == nil && result != nil` guard.
fn bfs_cross_links(
    graph: &GraphIndex,
    entity: &Entity,
    depth: u32,
) -> (Vec<EntityBrief>, HashMap<i64, CrossDomainLink>) {
    let mut related = Vec::new();
    let mut cross_by_target: HashMap<i64, CrossDomainLink> = HashMap::new();

    let Some(graph) = graph.ready() else {
        return (related, cross_by_target);
    };
    let options = TraverseOptions {
        max_depth: depth,
        follow_entity_links: true,
        max_nodes: BFS_MAX_NODES,
        ..Default::default()
    };
    let Ok(result) = graph.traverse(entity.id, &options) else {
        return (related, cross_by_target);
    };

    for node in &result.nodes {
        if node.id != entity.id {
            related.push(node_brief(node));
        }
    }

    // Cross-domain links come from the BFS edges INCIDENT to the center whose
    // target domain differs from the center's (the oracle's quirk: the
    // normalized node domain vs the raw entity domain).
    let node_map: HashMap<i64, &EntityNode> = result.nodes.iter().map(|n| (n.id, n)).collect();
    for edge in &result.edges {
        if edge.source != entity.id && edge.target != entity.id {
            continue;
        }
        let target_id = if edge.source == entity.id {
            edge.target
        } else {
            edge.source
        };
        let Some(target_node) = node_map.get(&target_id) else {
            continue;
        };
        if target_node.domain == entity.domain {
            continue;
        }
        let rel_type = edge.relation_type.clone();
        let candidate = CrossDomainLink {
            target_entity_id: target_id,
            target_name: target_node.name.clone(),
            target_domain: target_node.domain.clone(),
            relation_types: vec![rel_type.clone()],
            method: edge.method.clone(),
            confidence: edge.confidence,
            evidence: edge.evidence.clone(),
        };
        merge_cross_link(&mut cross_by_target, candidate, rel_type);
    }

    (related, cross_by_target)
}

/// Merge one freshly built link (carrying exactly one relation type) into the
/// target-deduplicated map: the entry with the better provenance is kept, and
/// the relation types are the union in encounter order (oracle
/// `hasBetterProvenance` + `mergeRelationTypes`).
fn merge_cross_link(
    map: &mut HashMap<i64, CrossDomainLink>,
    mut candidate: CrossDomainLink,
    rel_type: String,
) {
    let target_id = candidate.target_entity_id;
    match map.get_mut(&target_id) {
        Some(existing) => {
            if has_better_provenance(&candidate, existing) {
                let mut types = existing.relation_types.clone();
                push_unique(&mut types, rel_type);
                candidate.relation_types = types;
                map.insert(target_id, candidate);
            } else {
                push_unique(&mut existing.relation_types, rel_type);
            }
        }
        None => {
            map.insert(target_id, candidate);
        }
    }
}

/// Append `rel_type` to `types` if it is not already present (encounter order).
fn push_unique(types: &mut Vec<String>, rel_type: String) {
    if !types.iter().any(|t| t == &rel_type) {
        types.push(rel_type);
    }
}

/// Whether `candidate` has more complete provenance than `existing` (oracle
/// `hasBetterProvenance`): higher confidence wins, then a non-empty method over
/// an empty one, then a non-empty evidence, then a deterministic lexicographic
/// tiebreak on method, evidence, and target id.
fn has_better_provenance(candidate: &CrossDomainLink, existing: &CrossDomainLink) -> bool {
    let cand_conf = candidate.confidence.unwrap_or(0.0);
    let exist_conf = existing.confidence.unwrap_or(0.0);
    if cand_conf > exist_conf {
        return true;
    }
    if cand_conf < exist_conf {
        return false;
    }
    let cand_method = candidate.method.as_deref().unwrap_or_default();
    let exist_method = existing.method.as_deref().unwrap_or_default();
    if !cand_method.is_empty() && exist_method.is_empty() {
        return true;
    }
    if cand_method.is_empty() && !exist_method.is_empty() {
        return false;
    }
    let cand_evidence = candidate.evidence.as_deref().unwrap_or_default();
    let exist_evidence = existing.evidence.as_deref().unwrap_or_default();
    if !cand_evidence.is_empty() && exist_evidence.is_empty() {
        return true;
    }
    if cand_evidence.is_empty() && !exist_evidence.is_empty() {
        return false;
    }
    if cand_method != exist_method {
        return cand_method < exist_method;
    }
    if cand_evidence != exist_evidence {
        return cand_evidence < exist_evidence;
    }
    candidate.target_entity_id < existing.target_entity_id
}

// ── handler ──────────────────────────────────────────────────────────────────

/// Handle the `get_entity_dossier` tool call (design D2/D4).
///
/// `db` and `graph` are the injected collaborators; `args` is the raw JSON
/// argument object (`None` = no arguments). The result is the oracle-shaped
/// payload the server serializes into the tool response's text block.
pub fn handle_get_entity_dossier(
    db: &db::Db,
    graph: &GraphIndex,
    args: Option<&Value>,
) -> Result<Value, McpError> {
    let args: DossierArgs = deserialize_args(args, GET_ENTITY_DOSSIER)?;

    let entity = resolve_entity(db, &args)?;
    let depth = parse_depth(args.depth.as_ref());
    let include_facts = args.include_facts.unwrap_or(true);
    let include_sources = args.include_sources.unwrap_or(true);

    // Approved facts (DAO enforces `status = 'approved'`), newest first,
    // truncated to the cap, each with its sources.
    let facts = if include_facts {
        db.with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            let mut facts = FactDao::new(exec).list_by_entity_id(entity.id)?;
            facts.truncate(MAX_FACTS);
            let sources = FactSourceDao::new(exec);
            let mut out = Vec::with_capacity(facts.len());
            for fact in &facts {
                let fact_sources = sources.get_by_fact_id(fact.id)?;
                out.push(dossier_fact(fact, &fact_sources));
            }
            Ok(out)
        })
        .and_then(|result| result)?
    } else {
        Vec::new()
    };

    // Source documents: the entity's linked document ids, resolved and kept in
    // the oracle's id order (missing documents are skipped).
    let sources = if include_sources {
        db.with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            let doc_ids = EntitySourceDao::new(exec).get_documents_by_entity_id(entity.id)?;
            if doc_ids.is_empty() {
                return Ok(Vec::new());
            }
            let docs = DocumentDao::new(exec).get_by_ids(&doc_ids)?;
            let doc_map: HashMap<i64, &Document> = docs.iter().map(|d| (d.id, d)).collect();
            let mut out = Vec::with_capacity(doc_ids.len());
            for id in doc_ids {
                if let Some(doc) = doc_map.get(&id) {
                    out.push(source_doc(doc));
                }
            }
            Ok(out)
        })
        .and_then(|result| result)?
    } else {
        Vec::new()
    };

    // Related entities + cross-domain links via BFS (graph, no db).
    let (related_entities, mut cross_by_target) = bfs_cross_links(graph, &entity, depth);

    // Direct entity links (cross-domain). The oracle applies NO domain filter
    // here (recorded deviation): a same-domain link is kept.
    let direct = db
        .with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            let links = EntityLinkDao::new(exec).list_by_entity(entity.id)?;
            let entities = EntityDao::new(exec);
            let mut out = Vec::with_capacity(links.len());
            for link in links {
                let target_id = if link.subject_entity_id == entity.id {
                    link.target_entity_id
                } else {
                    link.subject_entity_id
                };
                let Some(target) = entities.get_by_id(target_id)? else {
                    continue;
                };
                let rel_type = link.relation_type.clone();
                out.push((
                    CrossDomainLink {
                        target_entity_id: target_id,
                        target_name: target.name.clone(),
                        target_domain: target.domain.clone(),
                        relation_types: vec![rel_type.clone()],
                        method: Some(link.method.clone()),
                        confidence: Some(link.confidence),
                        evidence: link.evidence.clone(),
                    },
                    rel_type,
                ));
            }
            Ok(out)
        })
        .and_then(|result| result)?;

    for (link, rel_type) in direct {
        merge_cross_link(&mut cross_by_target, link, rel_type);
    }

    // Deterministic output order (oracle `slices.Sort` by target id).
    let mut cross_domain_links: Vec<CrossDomainLink> = cross_by_target.into_values().collect();
    cross_domain_links.sort_by_key(|link| link.target_entity_id);

    let response = DossierResponse {
        entity: dossier_entity(&entity),
        facts,
        sources,
        related_entities,
        cross_domain_links,
    };
    to_value(GET_ENTITY_DOSSIER, response)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use config::preset::GraphConfig;
    use db::EntityLink;
    use db::test_util;

    use super::*;

    /// Seed an in-memory KB in one transaction; returns the db and the value
    /// the seeder produced (usually the created ids).
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

    fn call(db: &db::Db, graph: &GraphIndex, args: Option<Value>) -> Result<Value, McpError> {
        handle_get_entity_dossier(db, graph, args.as_ref())
    }

    /// The oracle `TestHandleGetEntityDossier` fixture: Alice (hr) `works_at`
    /// Acme (hr) + a fact source + a linked document.
    fn seeded_dossier_db() -> (db::Db, (i64, i64, i64)) {
        seed_db(|exec| {
            let entities = EntityDao::new(exec);
            let facts = FactDao::new(exec);
            let sources = FactSourceDao::new(exec);
            let documents = DocumentDao::new(exec);
            let entity_sources = EntitySourceDao::new(exec);
            let alice = entities.create("PERSON", "Alice", "hr", None, None, None)?;
            let acme = entities.create("ORGANIZATION", "Acme Corp", "hr", None, None, None)?;
            // The document must exist before the fact source references it
            // (`fact_sources.document_id` is a foreign key).
            let doc_id = documents.create("markdown", "/docs/hr.md", None, None)?;
            let fact_id =
                facts.create(Some(alice), "works_at", Some(acme), "hr", None, None, None)?;
            let quote = "Alice works at Acme Corp";
            sources.create(fact_id, doc_id, Some(quote), None)?;
            entity_sources.create(alice, doc_id)?;
            Ok((alice, acme, doc_id))
        })
    }

    // ── resolution (oracle TestHandleGetEntityDossier + _DomainDisambiguation)

    /// Oracle "empty/missing entity id returns error": the XOR is enforced.
    #[test]
    fn resolution_requires_exactly_one_of_id_or_name() {
        let (db, _) = seeded_dossier_db();
        let graph = GraphIndex::Unavailable;
        for args in [
            None,
            Some(serde_json::json!({})),
            Some(serde_json::json!({ "entity_id": "" })),
            Some(serde_json::json!({ "entity_id": "1", "entity_name": "Alice" })),
        ] {
            let err = call(&db, &graph, args).unwrap_err();
            assert!(
                matches!(err, McpError::InvalidArguments { .. }),
                "got: {err:?}"
            );
        }
    }

    /// Oracle "non-integer entity id returns error".
    #[test]
    fn resolution_non_integer_id_is_an_error() {
        let (db, _) = seeded_dossier_db();
        let graph = GraphIndex::Unavailable;
        let err = call(&db, &graph, Some(serde_json::json!({ "entity_id": "abc" }))).unwrap_err();
        assert!(
            matches!(err, McpError::InvalidArguments { .. }),
            "got: {err:?}"
        );
        assert!(err.to_string().contains("must be an integer"), "got: {err}");
    }

    /// Oracle "nonexistent entity id returns error" (recorded deviation: the
    /// message says "id", not the oracle's "Predicate").
    #[test]
    fn resolution_nonexistent_id_is_not_found() {
        let (db, _) = seeded_dossier_db();
        let graph = GraphIndex::Unavailable;
        let err = call(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_id": "99999" })),
        )
        .unwrap_err();
        assert!(matches!(err, McpError::NotFound { .. }), "got: {err:?}");
        assert!(
            err.to_string().contains("entity with id 99999 not found"),
            "got: {err}"
        );
    }

    /// Oracle `TestHandleGetEntityDossier_DomainDisambiguation`: two same-named
    /// entities in different domains.
    #[test]
    fn resolution_domain_disambiguates_and_lists_candidates() {
        let (db, ids) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            let id_hr = entities.create("PERSON", "Alice", "hr", None, None, None)?;
            let id_product = entities.create("PERSON", "Alice", "product", None, None, None)?;
            let unique = entities.create("PERSON", "UniqueBob", "hr", None, None, None)?;
            Ok((id_hr, id_product, unique))
        });
        let graph = GraphIndex::Unavailable;

        // A domain narrows to one entity (case-insensitive).
        let by_hr = call(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_name": "Alice", "domain": "hr" })),
        )
        .unwrap();
        assert_eq!(by_hr["entity"]["id"], ids.0);

        let by_product = call(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_name": "Alice", "domain": "PRODUCT" })),
        )
        .unwrap();
        assert_eq!(by_product["entity"]["id"], ids.1);

        // No domain + multiple matches lists the candidates.
        let ambiguous = call(
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

        // No domain + a single match succeeds.
        let single = call(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_name": "UniqueBob" })),
        )
        .unwrap();
        assert_eq!(single["entity"]["id"], ids.2);
    }

    // ── response shape (oracle TestHandleGetEntityDossier_ResponseFields) ────

    /// Oracle "valid entity returns dossier with facts and sources" + the
    /// response-fields test: entity fields, a fact, and a source.
    #[test]
    fn dossier_returns_entity_facts_and_sources() {
        let (db, (alice, _acme, doc_id)) = seeded_dossier_db();
        let graph = GraphIndex::Unavailable;

        let response = call(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_id": alice.to_string() })),
        )
        .unwrap();

        assert_eq!(response["entity"]["id"], alice);
        assert_eq!(response["entity"]["name"], "Alice");
        assert_eq!(response["entity"]["type"], "PERSON");
        assert_eq!(response["entity"]["domain"], "hr");

        let facts = response["facts"].as_array().unwrap();
        assert_eq!(facts.len(), 1, "{response}");
        assert_eq!(facts[0]["predicate"], "works_at");
        assert_eq!(facts[0]["status"], "approved");
        assert_eq!(facts[0]["domain"], "hr");
        assert_eq!(facts[0]["sources"].as_array().unwrap().len(), 1);
        assert_eq!(facts[0]["sources"][0]["quote"], "Alice works at Acme Corp");

        let sources = response["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 1, "{response}");
        assert_eq!(sources[0]["id"], doc_id);
        assert_eq!(sources[0]["source_type"], "markdown");
        assert_eq!(sources[0]["original_path"], "/docs/hr.md");
    }

    /// Oracle `TestHandleGetEntityDossier_ResponseFields` with description +
    /// metadata: both are present (the raw metadata string, not parsed).
    #[test]
    fn dossier_entity_carries_description_and_raw_metadata() {
        let (db, alice) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            let alice = entities.create(
                "PERSON",
                "Alice",
                "hr",
                Some("Senior engineer"),
                Some(0.9),
                Some(r#"{"role":"senior_engineer"}"#),
            )?;
            Ok(alice)
        });
        let graph = GraphIndex::Unavailable;

        let response = call(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_id": alice.to_string() })),
        )
        .unwrap();
        assert_eq!(response["entity"]["description"], "Senior engineer");
        assert_eq!(response["entity"]["confidence"], 0.9);
        assert_eq!(
            response["entity"]["metadata"], r#"{"role":"senior_engineer"}"#,
            "metadata must be the raw string, not parsed"
        );
    }

    /// Oracle `TestHandleGetEntityDossier_ExcludeFactsAndSources`: both flags
    /// false → the sections are omitted (omitempty).
    #[test]
    fn dossier_excludes_facts_and_sources_when_disabled() {
        let (db, (alice, _, _)) = seeded_dossier_db();
        let graph = GraphIndex::Unavailable;

        let response = call(
            &db,
            &graph,
            Some(serde_json::json!({
                "entity_id": alice.to_string(),
                "include_facts": false,
                "include_sources": false,
            })),
        )
        .unwrap();
        assert!(response.get("facts").is_none(), "{response}");
        assert!(response.get("sources").is_none(), "{response}");
        assert_eq!(response["entity"]["id"], alice);
    }

    /// Oracle `TestHandleGetEntityDossier_DepthClamping`: out-of-range depths
    /// are clamped, not rejected.
    #[test]
    fn dossier_depth_is_clamped_not_rejected() {
        let (db, alice) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            entities.create("PERSON", "Alice", "hr", None, None, None)
        });
        let graph = ready_graph(&db);
        for depth in [
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!(10),
            serde_json::json!("3"),
        ] {
            let response = call(
                &db,
                &graph,
                Some(serde_json::json!({ "entity_id": alice.to_string(), "depth": depth })),
            )
            .unwrap();
            assert_eq!(response["entity"]["id"], alice, "depth {depth}");
        }
    }

    /// An entity with no facts/sources/links yields a dossier with only the
    /// `entity` section (all others omitted).
    #[test]
    fn dossier_empty_sections_are_omitted() {
        let (db, alice) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            entities.create("PERSON", "Lonely", "hr", None, None, None)
        });
        let graph = ready_graph(&db);

        let response = call(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_id": alice.to_string() })),
        )
        .unwrap();
        assert_eq!(response["entity"]["id"], alice);
        for section in ["facts", "sources", "related_entities", "cross_domain_links"] {
            assert!(
                response.get(section).is_none(),
                "{section} must be omitted: {response}"
            );
        }
    }

    // ── cross-domain links (oracle TestCrossDomainLinks) ─────────────────────

    /// Oracle `FilterSameDomain` (with graph): same-domain targets stay out of
    /// `cross_domain_links` but appear in `related_entities`; cross-domain
    /// targets appear in both.
    #[test]
    fn cross_links_filter_same_domain() {
        let (db, ids) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            let facts = FactDao::new(exec);
            let links = EntityLinkDao::new(exec);
            let id_hr = entities.create("PERSON", "Alice", "hr", None, None, None)?;
            let same = entities.create("ORGANIZATION", "Acme HR", "hr", None, None, None)?;
            facts.create(Some(id_hr), "works_at", Some(same), "hr", None, None, None)?;
            let id_product =
                entities.create("POLICY", "Hiring Policy", "product", None, None, None)?;
            let id_it = entities.create("POLICY", "IT Policy", "it", None, None, None)?;
            links.create(&EntityLink {
                subject_entity_id: id_hr,
                target_entity_id: id_product,
                relation_type: "related_to".into(),
                method: "rule".into(),
                confidence: 0.95,
                evidence: None,
            })?;
            links.create(&EntityLink {
                subject_entity_id: id_hr,
                target_entity_id: id_it,
                relation_type: "related_to".into(),
                method: "equals".into(),
                confidence: 0.85,
                evidence: None,
            })?;
            Ok((id_hr, id_product, id_it))
        });
        let graph = ready_graph(&db);

        let response = call(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_id": ids.0.to_string() })),
        )
        .unwrap();

        let cross = response["cross_domain_links"].as_array().unwrap();
        let domains: std::collections::HashSet<&str> = cross
            .iter()
            .map(|link| link["target_domain"].as_str().unwrap())
            .collect();
        assert!(!domains.contains("hr"), "no same-domain entry: {response}");
        assert!(domains.contains("product"), "{response}");
        assert!(domains.contains("it"), "{response}");

        // The same-domain fact neighbor is a related entity, not a cross link.
        let related = response["related_entities"].as_array().unwrap();
        assert!(
            related
                .iter()
                .any(|node| node["domain"] == "hr" && node["id"] != ids.0),
            "a same-domain related entity is expected: {response}"
        );
    }

    /// Oracle `DedupByTargetEntityID` (no graph): two links to the same target
    /// collapse to one, keeping the higher confidence and both relation types.
    #[test]
    fn cross_links_dedup_by_target_and_keep_best_provenance() {
        let (db, (id_hr, id_product)) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            let links = EntityLinkDao::new(exec);
            let id_hr = entities.create("PERSON", "Alice", "hr", None, None, None)?;
            let id_product =
                entities.create("POLICY", "Hiring Policy", "product", None, None, None)?;
            links.create(&EntityLink {
                subject_entity_id: id_hr,
                target_entity_id: id_product,
                relation_type: "related_to".into(),
                method: "rule".into(),
                confidence: 0.7,
                evidence: None,
            })?;
            links.create(&EntityLink {
                subject_entity_id: id_hr,
                target_entity_id: id_product,
                relation_type: "equals".into(),
                method: "llm".into(),
                confidence: 0.95,
                evidence: None,
            })?;
            Ok((id_hr, id_product))
        });
        let graph = GraphIndex::Unavailable;

        let response = call(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_id": id_hr.to_string() })),
        )
        .unwrap();

        let cross = response["cross_domain_links"].as_array().unwrap();
        assert_eq!(cross.len(), 1, "one entry per target: {response}");
        let link = &cross[0];
        assert_eq!(link["target_entity_id"], id_product);
        assert_eq!(link["confidence"], 0.95, "the higher confidence is kept");
        let types: Vec<&str> = link["relation_types"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(types.contains(&"related_to"), "{types:?}");
        assert!(types.contains(&"equals"), "{types:?}");
    }

    /// Oracle `IncomingLink` (no graph): a link where the entity is the target
    /// resolves the subject as the cross-domain target.
    #[test]
    fn cross_links_resolve_incoming_links() {
        let (db, id_hr) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            let links = EntityLinkDao::new(exec);
            let policy = entities.create("POLICY", "Hiring Policy", "product", None, None, None)?;
            let id_hr = entities.create("PERSON", "Alice", "hr", None, None, None)?;
            links.create(&EntityLink {
                subject_entity_id: policy,
                target_entity_id: id_hr,
                relation_type: "related_to".into(),
                method: "rule".into(),
                confidence: 0.95,
                evidence: None,
            })?;
            Ok(id_hr)
        });
        let graph = GraphIndex::Unavailable;

        let response = call(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_id": id_hr.to_string() })),
        )
        .unwrap();
        let cross = response["cross_domain_links"].as_array().unwrap();
        assert!(
            cross
                .iter()
                .any(|link| link["target_name"] == "Hiring Policy"
                    && link["target_domain"] == "product"),
            "the incoming link must resolve: {response}"
        );
    }

    /// Oracle `RelationTypesMerging` (no graph): per target the relation types
    /// are the union, and the best-provenance entry's confidence is kept.
    #[test]
    fn cross_links_merge_relation_types_per_target() {
        let (db, id_hr) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            let links = EntityLinkDao::new(exec);
            let id_hr = entities.create("PERSON", "Alice", "hr", None, None, None)?;
            let product =
                entities.create("POLICY", "Hiring Policy", "product", None, None, None)?;
            let server = entities.create("SERVER", "Server A", "it", None, None, None)?;
            for (target, rel, method, conf) in [
                (product, "related_to", "rule", 0.95),
                (product, "equals", "llm", 0.85),
                (product, "related_to", "equals", 0.6),
                (server, "manages", "rule", 0.9),
            ] {
                links.create(&EntityLink {
                    subject_entity_id: id_hr,
                    target_entity_id: target,
                    relation_type: rel.into(),
                    method: method.into(),
                    confidence: conf,
                    evidence: None,
                })?;
            }
            Ok(id_hr)
        });
        let graph = GraphIndex::Unavailable;

        let response = call(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_id": id_hr.to_string() })),
        )
        .unwrap();
        let cross = response["cross_domain_links"].as_array().unwrap();
        assert_eq!(cross.len(), 2, "one entry per target: {response}");

        let product = cross
            .iter()
            .find(|link| link["target_domain"] == "product")
            .unwrap();
        let product_types: Vec<&str> = product["relation_types"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(product_types.contains(&"related_to"), "{product_types:?}");
        assert!(product_types.contains(&"equals"), "{product_types:?}");
        assert_eq!(product["confidence"], 0.95);

        let server = cross
            .iter()
            .find(|link| link["target_domain"] == "it")
            .unwrap();
        assert_eq!(server["relation_types"], serde_json::json!(["manages"]));
    }

    /// Oracle `BFSOnlyIncidentEdges` (with graph): only edges incident to the
    /// center produce cross links; a depth-2 hop is not.
    #[test]
    fn cross_links_bfs_only_incident_edges() {
        let (db, id_hr) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            let links = EntityLinkDao::new(exec);
            let id_hr = entities.create("PERSON", "Alice", "hr", None, None, None)?;
            let policy_a = entities.create("POLICY", "Policy A", "product", None, None, None)?;
            let policy_b = entities.create("POLICY", "Policy B", "product", None, None, None)?;
            links.create(&EntityLink {
                subject_entity_id: id_hr,
                target_entity_id: policy_a,
                relation_type: "related_to".into(),
                method: "rule".into(),
                confidence: 0.95,
                evidence: None,
            })?;
            links.create(&EntityLink {
                subject_entity_id: policy_a,
                target_entity_id: policy_b,
                relation_type: "related_to".into(),
                method: "rule".into(),
                confidence: 0.85,
                evidence: None,
            })?;
            Ok(id_hr)
        });
        let graph = ready_graph(&db);

        let response = call(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_id": id_hr.to_string() })),
        )
        .unwrap();
        let cross = response["cross_domain_links"].as_array().unwrap();
        assert!(
            cross.iter().any(|link| link["target_name"] == "Policy A"),
            "the incident link is expected: {response}"
        );
        assert!(
            !cross.iter().any(|link| link["target_name"] == "Policy B"),
            "a non-incident hop must not be a cross link: {response}"
        );
    }

    /// Oracle `RelationTypesFromBFS` (with graph): the BFS edge's relation type
    /// and the direct link's type both land in the same target's union.
    #[test]
    fn cross_links_merge_bfs_and_direct_types() {
        let (db, id_hr) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            let links = EntityLinkDao::new(exec);
            let id_hr = entities.create("PERSON", "Alice", "hr", None, None, None)?;
            let product =
                entities.create("POLICY", "Hiring Policy", "product", None, None, None)?;
            links.create(&EntityLink {
                subject_entity_id: id_hr,
                target_entity_id: product,
                relation_type: "related_to".into(),
                method: "rule".into(),
                confidence: 0.95,
                evidence: None,
            })?;
            links.create(&EntityLink {
                subject_entity_id: id_hr,
                target_entity_id: product,
                relation_type: "equals".into(),
                method: "llm".into(),
                confidence: 0.85,
                evidence: None,
            })?;
            Ok(id_hr)
        });
        let graph = ready_graph(&db);

        let response = call(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_id": id_hr.to_string() })),
        )
        .unwrap();
        let cross = response["cross_domain_links"].as_array().unwrap();
        assert_eq!(cross.len(), 1, "one entry per target: {response}");
        let types: Vec<&str> = cross[0]["relation_types"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(types.contains(&"related_to"), "{types:?}");
        assert!(types.contains(&"equals"), "{types:?}");
    }

    /// Oracle `ProvenancePresent` (with graph): a cross-domain link carries its
    /// method and confidence.
    #[test]
    fn cross_links_carry_provenance() {
        let (db, id_hr) = seed_db(|exec| {
            let entities = EntityDao::new(exec);
            let links = EntityLinkDao::new(exec);
            let id_hr = entities.create("PERSON", "Alice", "hr", None, None, None)?;
            let product =
                entities.create("POLICY", "Hiring Policy", "product", None, None, None)?;
            links.create(&EntityLink {
                subject_entity_id: id_hr,
                target_entity_id: product,
                relation_type: "equals".into(),
                method: "rule".into(),
                confidence: 0.95,
                evidence: Some("name match with confidence 0.95".into()),
            })?;
            Ok(id_hr)
        });
        let graph = ready_graph(&db);

        let response = call(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_id": id_hr.to_string() })),
        )
        .unwrap();
        let cross = response["cross_domain_links"].as_array().unwrap();
        let link = cross
            .iter()
            .find(|link| link["target_domain"] == "product")
            .unwrap_or_else(|| panic!("a product cross link is expected: {response}"));
        assert_eq!(link["method"], "rule");
        assert_eq!(link["confidence"], 0.95);
        assert_eq!(link["evidence"], "name match with confidence 0.95");
    }
}
