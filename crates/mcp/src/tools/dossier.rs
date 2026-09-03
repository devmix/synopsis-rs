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
