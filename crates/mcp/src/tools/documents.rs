//! The `get_document_context` and `get_chunk_by_id` tools: the full document
//! context (metadata, chunks, entities, fact ids) and the single-chunk lookup
//! with document info and entities (design D4).
//!
//! Oracle mapping: `../synopsis/internal/mcp/handlers/{get_document_context.go,
//! get_chunk_by_id.go}`.
//!
//! Thin handlers per design D2: parse the frozen-schema arguments → db DAOs
//! → oracle-shaped JSON. No business logic lives here.
//!
//! **Recorded deviations:**
//! 1. *Error text:* the oracle prefixes its tool-error messages with
//!    `"Error …"`; this crate uses the [`McpError`] conventions established
//!    by tasks 5.1/5.3/5.6 (e.g. `"invalid arguments for tool
//!    'get_document_context': …"`). The tool-error structure (is_error result
//!    with a text block) is the same; internal message text is not part of
//!    the frozen contract.
//! 2. *Not-found messages (bug fix):* the oracle's messages are `"Document
//!    with Predicate %d not found"` / `"Chunk with Predicate %d not found"` —
//!    they say "Predicate" where the id is meant. This crate says
//!    `"document with id N not found"` / `"chunk with id N not found"`.
//! 3. *`fact_ids` order:* the oracle deduplicated fact ids while iterating a
//!    Go `map[int][]Fact` — a random order per run. This crate derives them
//!    from the deterministic de-duplicated entity-id list (first-occurrence
//!    order across chunks, by entity id when chunks are not loaded).
//! 4. *Entity lookup errors:* the oracle's `collectAllEntityIDs` skipped
//!    per-chunk lookup errors silently; this crate propagates them (the db
//!    crate's batched `get_entities_by_chunks` makes the per-chunk loop
//!    obsolete — one query, one error path).
//!
//! **Response parity:** field names, order and optionality match the Go
//! structs (`DocumentContextResponse`/`DocumentInfo`/`ChunkWithContext`,
//! `ChunkByIDResponse`/`ChunkInfo`/`DocumentBrief`, `EntityWithContext`), so
//! the wire JSON matches the oracle's marshal output. `ChunkWithContext`
//! (document context) always carries `start_offset`/`end_offset` (0 when
//! NULL), while `ChunkInfo` (chunk by id) omits them when NULL — the oracle's
//! two structs differ in exactly that, and the difference is preserved.

use std::collections::{HashMap, HashSet};

use db::{
    Chunk, ChunkDao, ChunkEntityDao, ConnectionOrTx, Document, DocumentDao, Entity, EntityDao,
    FactDao,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::error::McpError;
use crate::tools::entity::{EntityBrief, entity_brief};

/// The frozen tool name (`mcp-contract`).
pub const GET_DOCUMENT_CONTEXT: &str = "get_document_context";
/// The frozen tool name (`mcp-contract`).
pub const GET_CHUNK_BY_ID: &str = "get_chunk_by_id";

/// The metadata keys scanned for image paths (oracle `extractImagePaths`).
const IMAGE_KEYS: [&str; 3] = ["images", "image_paths", "attachments"];

// ── shared helpers ──────────────────────────────────────────────────────────

/// Deserialize the argument object; `None` (no arguments) is the empty
/// object. Unknown keys are ignored, as in the oracle's `req.Get*` accessors
/// (same convention as `tools::facts`).
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
/// are primitives), but the no-panic rule (design D7) keeps an error path
/// instead of a panic (same convention as `tools::facts`).
fn to_value<T: Serialize>(tool: &'static str, response: T) -> Result<Value, McpError> {
    serde_json::to_value(response).map_err(|err| McpError::InvalidArguments {
        tool,
        reason: format!("serializing the response failed: {err}"),
    })
}

/// Parse a required integer-as-string id argument (oracle
/// `strconv.Atoi`-after-`GetString`); same convention as `tools::facts`.
fn parse_id_arg(raw: String, key: &str, tool: &'static str) -> Result<i64, McpError> {
    if raw.is_empty() {
        return Err(McpError::InvalidArguments {
            tool,
            reason: format!("'{key}' argument is required"),
        });
    }
    raw.parse().map_err(|_| McpError::InvalidArguments {
        tool,
        reason: format!("'{key}' must be an integer, got {raw:?}"),
    })
}

/// De-duplicate `entity_ids` keeping first-occurrence order, then resolve
/// them to full rows in that same order (ids with no row are skipped —
/// oracle `GetByIDs` map-lookup semantics).
fn resolve_entities(
    exec: ConnectionOrTx<'_>,
    entity_ids: &[i64],
) -> Result<Vec<Entity>, db::DbError> {
    if entity_ids.is_empty() {
        return Ok(Vec::new());
    }
    // First-occurrence order over the de-duplicated ids.
    let mut seen = HashSet::new();
    let ordered: Vec<i64> = entity_ids
        .iter()
        .copied()
        .filter(|id| seen.insert(*id))
        .collect();
    let rows = EntityDao::new(exec).get_by_ids(&ordered)?;
    let by_id: HashMap<i64, Entity> = rows.into_iter().map(|entity| (entity.id, entity)).collect();
    let mut entities = Vec::with_capacity(ordered.len());
    for id in &ordered {
        if let Some(entity) = by_id.get(id) {
            entities.push(entity.clone());
        }
    }
    Ok(entities)
}

// ── get_document_context ────────────────────────────────────────────────────

/// Parsed `get_document_context` arguments (frozen schema: `document_id`,
/// `include_chunks`, `include_entities`, `include_facts`).
#[derive(Debug, Deserialize)]
struct DocumentContextArgs {
    /// The document id (required, integer as string).
    document_id: Option<String>,
    /// Include chunk data (default `true`).
    include_chunks: Option<bool>,
    /// Include entities (default `true`).
    include_entities: Option<bool>,
    /// Include fact ids (default `false`).
    include_facts: Option<bool>,
}

/// The document metadata (oracle `DocumentInfo`): field order matches the Go
/// struct.
#[derive(Debug, Serialize)]
struct DocumentInfo {
    /// Document row id.
    id: i64,
    /// Originating source.
    source_type: String,
    /// Path of the original file.
    original_path: String,
    /// The raw `metadata_json` string; absent when the document has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<String>,
    /// Creation timestamp.
    created_at: String,
    /// Last-update timestamp.
    updated_at: String,
    /// Domains from `metadata_json` `$.domain` (string or array member);
    /// absent when the metadata has none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    domains: Vec<String>,
}

/// A chunk of the document (oracle `ChunkWithContext`): field order matches
/// the Go struct. Unlike `ChunkInfo` (chunk by id), the offsets are ALWAYS
/// present (0 when NULL) — the oracle struct has no omitempty on them.
#[derive(Debug, Serialize)]
struct ChunkWithContext {
    /// Chunk row id.
    id: i64,
    /// Position within the document.
    sequence_num: i64,
    /// Start offset in the original text (0 when absent).
    start_offset: i64,
    /// End offset in the original text (0 when absent).
    end_offset: i64,
    /// The chunk text.
    text: String,
}

/// The `get_document_context` response (oracle
/// `DocumentContextResponse`): field order matches the Go struct.
#[derive(Debug, Serialize)]
struct DocumentContextResponse {
    /// The document metadata.
    document: DocumentInfo,
    /// Image paths extracted from the metadata (`images`/`image_paths`/
    /// `attachments` keys); absent when the metadata has none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    image_paths: Vec<String>,
    /// Number of chunks of the document (always present).
    chunk_count: i64,
    /// The chunks; absent when `include_chunks` is false or the document has
    /// none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    chunks: Vec<ChunkWithContext>,
    /// The entities mentioned by the document's chunks; absent when
    /// `include_entities` is false or there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    entities: Vec<EntityBrief>,
    /// Approved fact ids linked to the document's entities; absent when
    /// `include_facts` is false or there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    fact_ids: Vec<i64>,
}

/// Image paths from a document's raw metadata JSON (oracle
/// `extractImagePaths`): every non-empty string of the `images`,
/// `image_paths` and `attachments` array keys, concatenated in that key
/// order. Malformed or missing metadata yields none.
fn extract_image_paths(metadata_json: &Option<String>) -> Vec<String> {
    let Some(metadata) = metadata_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
    else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for key in IMAGE_KEYS {
        if let Some(items) = metadata.get(key).and_then(Value::as_array) {
            for item in items {
                if let Some(path) = item.as_str().filter(|path| !path.is_empty()) {
                    paths.push(path.to_owned());
                }
            }
        }
    }
    paths
}

/// Domains from a document's raw metadata JSON (oracle `$.domain` handling):
/// a non-empty string, or the non-empty string members of an array, in order.
/// Malformed or missing metadata yields none.
fn extract_domains(metadata_json: &Option<String>) -> Vec<String> {
    let Some(metadata) = metadata_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
    else {
        return Vec::new();
    };
    match metadata.get("domain") {
        Some(Value::String(domain)) if !domain.is_empty() => vec![domain.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .filter(|domain| !domain.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// Map a stored document to the wire `document` object.
fn document_info(doc: &Document) -> DocumentInfo {
    DocumentInfo {
        id: doc.id,
        source_type: doc.source_type.clone(),
        original_path: doc.original_path.clone(),
        metadata: doc
            .metadata_json
            .as_ref()
            .filter(|metadata| !metadata.is_empty())
            .cloned(),
        created_at: doc.created_at.clone(),
        updated_at: doc.updated_at.clone(),
        domains: extract_domains(&doc.metadata_json),
    }
}

/// Handle the `get_document_context` tool call (design D2/D4).
///
/// `args` is the raw JSON argument object (`None` = no arguments). The
/// result is the oracle-shaped payload the server serializes into the tool
/// response's text block.
pub fn handle_get_document_context(db: &db::Db, args: Option<&Value>) -> Result<Value, McpError> {
    let args: DocumentContextArgs = deserialize_args(args, GET_DOCUMENT_CONTEXT)?;
    let document_id = parse_id_arg(
        args.document_id.unwrap_or_default(),
        "document_id",
        GET_DOCUMENT_CONTEXT,
    )?;
    let include_chunks = args.include_chunks.unwrap_or(true);
    let include_entities = args.include_entities.unwrap_or(true);
    let include_facts = args.include_facts.unwrap_or(false);

    let (doc, chunk_count, chunks, entities, fact_ids) = db
        .with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            let Some(doc) = DocumentDao::new(exec).get_by_id(document_id)? else {
                return Ok(None);
            };
            let chunk_dao = ChunkDao::new(exec);
            let chunk_entity_dao = ChunkEntityDao::new(exec);

            // Chunks (or just their count when not requested).
            let chunks: Vec<Chunk> = if include_chunks {
                chunk_dao.list_by_doc_id(document_id)?
            } else {
                Vec::new()
            };
            let chunk_count = if include_chunks {
                chunks.len() as i64
            } else {
                chunk_dao.count_by_doc_id(document_id)?
            };

            // De-duplicated entity ids in first-occurrence order across
            // chunks (by entity id when chunks are not loaded — the oracle's
            // `GetEntityIDsByDocID` path); shared by the entities and facts
            // sections (DRY: one lookup, not two).
            let entity_ids: Vec<i64> = if include_entities || include_facts {
                if chunks.is_empty() {
                    chunk_entity_dao.get_entity_ids_by_doc_id(document_id)?
                } else {
                    let chunk_ids: Vec<i64> = chunks.iter().map(|chunk| chunk.id).collect();
                    let by_chunk = chunk_entity_dao.get_entities_by_chunks(&chunk_ids)?;
                    let mut seen = HashSet::new();
                    let mut ids = Vec::new();
                    for chunk in &chunks {
                        if let Some(entity_rows) = by_chunk.get(&chunk.id) {
                            for entity in entity_rows {
                                if seen.insert(entity.id) {
                                    ids.push(entity.id);
                                }
                            }
                        }
                    }
                    ids
                }
            } else {
                Vec::new()
            };

            let entities = resolve_entities(exec, &entity_ids)?;
            let entities = if include_entities {
                entities
            } else {
                Vec::new()
            };

            // Approved fact ids linked to the document's entities, in
            // deterministic first-occurrence order (recorded deviation 3).
            let fact_ids: Vec<i64> = if include_facts && !entity_ids.is_empty() {
                let grouped = FactDao::new(exec).list_by_entity_ids(&entity_ids)?;
                let mut seen_facts = HashSet::new();
                let mut ids = Vec::new();
                for id in &entity_ids {
                    if let Some(facts) = grouped.get(id) {
                        for fact in facts {
                            if seen_facts.insert(fact.id) {
                                ids.push(fact.id);
                            }
                        }
                    }
                }
                ids
            } else {
                Vec::new()
            };

            Ok(Some((doc, chunk_count, chunks, entities, fact_ids)))
        })
        .and_then(|result| result)?
        .ok_or_else(|| McpError::NotFound {
            what: format!("document with id {document_id} not found"),
        })?;

    let response = DocumentContextResponse {
        document: document_info(&doc),
        image_paths: extract_image_paths(&doc.metadata_json),
        chunk_count,
        chunks: chunks
            .into_iter()
            .map(|chunk| ChunkWithContext {
                id: chunk.id,
                sequence_num: chunk.sequence_num,
                start_offset: chunk.start_offset.unwrap_or(0),
                end_offset: chunk.end_offset.unwrap_or(0),
                text: chunk.chunk_text,
            })
            .collect(),
        entities: entities.iter().map(entity_brief).collect(),
        fact_ids,
    };
    to_value(GET_DOCUMENT_CONTEXT, response)
}

// ── get_chunk_by_id ─────────────────────────────────────────────────────────

/// Parsed `get_chunk_by_id` arguments (frozen schema: `chunk_id`).
#[derive(Debug, Deserialize)]
struct ChunkByIdArgs {
    /// The chunk id (required, integer as string).
    chunk_id: Option<String>,
}

/// The chunk data (oracle `ChunkInfo`): field order matches the Go struct.
#[derive(Debug, Serialize)]
struct ChunkInfo {
    /// Chunk row id.
    id: i64,
    /// Owning document id.
    doc_id: i64,
    /// The chunk text.
    chunk_text: String,
    /// Position within the document.
    sequence_num: i64,
    /// Start offset in the original text; absent when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    start_offset: Option<i64>,
    /// End offset in the original text; absent when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    end_offset: Option<i64>,
    /// Creation timestamp.
    created_at: String,
}

/// Minimal document metadata (oracle `DocumentBrief`).
#[derive(Debug, Serialize)]
struct DocumentBrief {
    /// Document row id.
    id: i64,
    /// Originating source.
    source_type: String,
    /// Path of the original file.
    original_path: String,
}

/// The `get_chunk_by_id` response (oracle `ChunkByIDResponse`): field order
/// matches the Go struct.
#[derive(Debug, Serialize)]
struct ChunkByIdResponse {
    /// The chunk data.
    chunk: ChunkInfo,
    /// The owning document; absent when it cannot be resolved (unreachable
    /// under the v5 schema's `doc_id` FK — oracle parity keeps the option).
    #[serde(skip_serializing_if = "Option::is_none")]
    document: Option<DocumentBrief>,
    /// The entities mentioned in the chunk; absent when there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    entities: Vec<EntityBrief>,
}

/// Handle the `get_chunk_by_id` tool call (design D2/D4).
///
/// `args` is the raw JSON argument object (`None` = no arguments). The
/// result is the oracle-shaped payload the server serializes into the tool
/// response's text block.
pub fn handle_get_chunk_by_id(db: &db::Db, args: Option<&Value>) -> Result<Value, McpError> {
    let args: ChunkByIdArgs = deserialize_args(args, GET_CHUNK_BY_ID)?;
    let chunk_id = parse_id_arg(
        args.chunk_id.unwrap_or_default(),
        "chunk_id",
        GET_CHUNK_BY_ID,
    )?;

    let (chunk, doc, entities) = db
        .with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            let Some(chunk) = ChunkDao::new(exec).get_by_id(chunk_id)? else {
                return Ok(None);
            };
            let doc = DocumentDao::new(exec).get_by_id(chunk.doc_id)?;
            let entity_ids = ChunkEntityDao::new(exec).get_entities_by_chunk(chunk_id)?;
            let entities = resolve_entities(exec, &entity_ids)?;
            Ok(Some((chunk, doc, entities)))
        })
        .and_then(|result| result)?
        .ok_or_else(|| McpError::NotFound {
            what: format!("chunk with id {chunk_id} not found"),
        })?;

    let response = ChunkByIdResponse {
        chunk: ChunkInfo {
            id: chunk.id,
            doc_id: chunk.doc_id,
            chunk_text: chunk.chunk_text,
            sequence_num: chunk.sequence_num,
            start_offset: chunk.start_offset,
            end_offset: chunk.end_offset,
            created_at: chunk.created_at,
        },
        document: doc.as_ref().map(|doc| DocumentBrief {
            id: doc.id,
            source_type: doc.source_type.clone(),
            original_path: doc.original_path.clone(),
        }),
        entities: entities.iter().map(entity_brief).collect(),
    };
    to_value(GET_CHUNK_BY_ID, response)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use db::{FactDao, test_util};

    use super::*;

    // ── fixtures (oracle get_document_context_test.go / get_chunk_by_id_test.go)

    /// A document with `metadata_json`; returns its id.
    fn seed_document(db: &db::Db, source_type: &str, path: &str, metadata: Option<&str>) -> i64 {
        db.with_conn(|conn| {
            DocumentDao::new(ConnectionOrTx::Connection(conn)).create(
                source_type,
                path,
                metadata,
                None,
            )
        })
        .expect("checkout")
        .expect("document insert")
    }

    /// A chunk of `doc_id`; returns its id.
    fn seed_chunk(
        db: &db::Db,
        doc_id: i64,
        text: &str,
        sequence: i64,
        offsets: (Option<i64>, Option<i64>),
    ) -> i64 {
        db.with_conn(|conn| {
            ChunkDao::new(ConnectionOrTx::Connection(conn))
                .create(doc_id, text, sequence, offsets.0, offsets.1)
        })
        .expect("checkout")
        .expect("chunk insert")
    }

    /// An entity; returns its id.
    fn seed_entity(db: &db::Db, entity_type: &str, name: &str, domain: &str) -> i64 {
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
        .expect("checkout")
        .expect("entity insert")
    }

    /// A chunk→entity link.
    fn link_chunk(db: &db::Db, chunk_id: i64, entity_id: i64) {
        db.with_conn(|conn| {
            ChunkEntityDao::new(ConnectionOrTx::Connection(conn)).link(chunk_id, entity_id)
        })
        .expect("checkout")
        .expect("chunk-entity link");
    }

    /// An approved fact; returns its id.
    fn seed_fact(db: &db::Db, subject: i64, predicate: &str, object: i64, domain: &str) -> i64 {
        db.with_conn(|conn| {
            FactDao::new(ConnectionOrTx::Connection(conn)).create(
                Some(subject),
                predicate,
                Some(object),
                domain,
                None,
                None,
                None,
            )
        })
        .expect("checkout")
        .expect("fact insert")
    }

    fn doc_context(db: &db::Db, args: Option<Value>) -> Result<Value, McpError> {
        handle_get_document_context(db, args.as_ref())
    }

    fn chunk_by_id(db: &db::Db, args: Option<Value>) -> Result<Value, McpError> {
        handle_get_chunk_by_id(db, args.as_ref())
    }

    // ── get_document_context: argument validation ─────────────────────────

    /// Oracle "empty document id returns error" + "missing document id
    /// returns error".
    #[test]
    fn doc_context_missing_document_id_is_an_error() {
        let db = test_util::in_memory_db();
        for args in [
            None,
            Some(serde_json::json!({})),
            Some(serde_json::json!({ "document_id": "" })),
        ] {
            let err = doc_context(&db, args).unwrap_err();
            assert!(
                matches!(err, McpError::InvalidArguments { .. }),
                "got: {err:?}"
            );
            assert!(
                err.to_string()
                    .contains("'document_id' argument is required"),
                "got: {err}"
            );
        }
    }

    /// Oracle "non-integer document id returns error".
    #[test]
    fn doc_context_non_integer_document_id_is_an_error() {
        let db = test_util::in_memory_db();
        for raw in ["not_a_number", "1.5", " 1"] {
            let err =
                doc_context(&db, Some(serde_json::json!({ "document_id": raw }))).unwrap_err();
            assert!(
                matches!(err, McpError::InvalidArguments { .. }),
                "got: {err:?}"
            );
            assert!(err.to_string().contains("must be an integer"), "got: {err}");
        }
    }

    // ── get_document_context: not-found ────────────────────────────────────

    /// Oracle "nonexistent document id returns error": not-found is a tool
    /// error (recorded deviation: the message says "id", not the oracle's
    /// "Predicate").
    #[test]
    fn doc_context_nonexistent_document_is_not_found() {
        let db = test_util::in_memory_db();
        let err =
            doc_context(&db, Some(serde_json::json!({ "document_id": "99999" }))).unwrap_err();
        assert!(matches!(err, McpError::NotFound { .. }), "got: {err:?}");
        assert!(
            err.to_string().contains("document with id 99999 not found"),
            "got: {err}"
        );
        let result = err.into_tool_result();
        let rmcp::model::CallToolResponse::Complete(call) = result else {
            panic!("expected a complete tool result");
        };
        assert_eq!(call.is_error, Some(true));
    }

    // ── get_document_context: full structure (oracle _ResponseFields) ─────

    /// Oracle `TestHandleGetDocumentContext_ResponseFields`: metadata with
    /// a `domain` array, three chunks, the full `document` object.
    #[test]
    fn doc_context_response_fields() {
        let db = test_util::in_memory_db();
        let doc_id = seed_document(
            &db,
            "json",
            "/data/policy.json",
            Some(r#"{"author":"test","domain":["hr"]}"#),
        );
        for (sequence, text) in [(1, "Chunk one."), (2, "Chunk two."), (3, "Chunk three.")] {
            seed_chunk(&db, doc_id, text, sequence, (None, None));
        }

        let response = doc_context(
            &db,
            Some(serde_json::json!({ "document_id": doc_id.to_string() })),
        )
        .unwrap();

        assert_eq!(response["document"]["id"], doc_id);
        assert_eq!(response["document"]["source_type"], "json");
        assert_eq!(response["document"]["original_path"], "/data/policy.json");
        assert_eq!(
            response["document"]["metadata"],
            r#"{"author":"test","domain":["hr"]}"#
        );
        assert_eq!(response["document"]["domains"], serde_json::json!(["hr"]));
        assert!(response["document"]["created_at"].is_string());
        assert!(response["document"]["updated_at"].is_string());
        assert_eq!(response["chunk_count"], serde_json::json!(3));
        let chunks = response["chunks"].as_array().unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0]["id"], response["chunks"][0]["id"]);
        assert_eq!(chunks[0]["sequence_num"], serde_json::json!(1));
        assert_eq!(chunks[0]["text"], "Chunk one.");
        // Oracle ChunkWithContext: offsets always present (0 when NULL).
        assert_eq!(chunks[0]["start_offset"], serde_json::json!(0));
        assert_eq!(chunks[0]["end_offset"], serde_json::json!(0));
        // No entities / facts seeded → the fields are omitted.
        assert!(response.get("entities").is_none(), "{response}");
        assert!(response.get("fact_ids").is_none(), "{response}");
        // No image keys in the metadata → the field is omitted.
        assert!(response.get("image_paths").is_none(), "{response}");
    }

    /// Oracle `TestHandleGetDocumentContext_NoMetadata` + the task's
    /// "document without chunks" criterion: a bare document is a valid
    /// response with `chunk_count = 0` and no optional fields.
    #[test]
    fn doc_context_document_without_metadata_or_chunks() {
        let db = test_util::in_memory_db();
        let doc_id = seed_document(&db, "markdown", "/docs/plain.md", None);

        let response = doc_context(
            &db,
            Some(serde_json::json!({ "document_id": doc_id.to_string() })),
        )
        .unwrap();

        assert_eq!(response["chunk_count"], serde_json::json!(0));
        assert!(response.get("chunks").is_none(), "{response}");
        assert!(response.get("metadata").is_none(), "{response}");
        assert!(response["document"].get("metadata").is_none(), "{response}");
        assert!(response["document"].get("domains").is_none(), "{response}");
        assert!(response.get("entities").is_none(), "{response}");
        assert!(response.get("fact_ids").is_none(), "{response}");
    }

    /// `image_paths` is extracted from the `images`/`image_paths`/
    /// `attachments` metadata keys (oracle `extractImagePaths`): every
    /// non-empty string of every key, concatenated in key order (the oracle
    /// does not validate that a value looks like a path).
    #[test]
    fn doc_context_image_paths_extracted_from_metadata() {
        let db = test_util::in_memory_db();
        let doc_id = seed_document(
            &db,
            "markdown",
            "/docs/pics.md",
            Some(
                r#"{"images":["/a.png",""], "image_paths":["/b.png"], "attachments":["/c.pdf","42"]}"#,
            ),
        );

        let response = doc_context(
            &db,
            Some(serde_json::json!({ "document_id": doc_id.to_string() })),
        )
        .unwrap();
        assert_eq!(
            response["image_paths"],
            serde_json::json!(["/a.png", "/b.png", "/c.pdf", "42"])
        );
    }

    /// `domains` also accepts a plain `domain` string (oracle switch-case).
    #[test]
    fn doc_context_domain_string_in_metadata() {
        let db = test_util::in_memory_db();
        let doc_id = seed_document(&db, "markdown", "/docs/hr.md", Some(r#"{"domain":"hr"}"#));

        let response = doc_context(
            &db,
            Some(serde_json::json!({ "document_id": doc_id.to_string() })),
        )
        .unwrap();
        assert_eq!(response["document"]["domains"], serde_json::json!(["hr"]));
    }

    // ── get_document_context: include_chunks=false (oracle _IncludeChunksFalse)

    /// Oracle `TestHandleGetDocumentContext_IncludeChunksFalse`: the count
    /// comes from a COUNT, the `chunks` array is omitted.
    #[test]
    fn doc_context_include_chunks_false_counts_without_loading() {
        let db = test_util::in_memory_db();
        let doc_id = seed_document(&db, "markdown", "/docs/test.md", None);
        seed_chunk(&db, doc_id, "First chunk.", 1, (None, None));
        seed_chunk(&db, doc_id, "Second chunk.", 2, (None, None));

        let response = doc_context(
            &db,
            Some(serde_json::json!({
                "document_id": doc_id.to_string(),
                "include_chunks": false,
                "include_entities": false,
            })),
        )
        .unwrap();

        assert_eq!(response["chunk_count"], serde_json::json!(2));
        assert!(response.get("chunks").is_none(), "{response}");
    }

    // ── get_document_context: entities (oracle _WithEntities) ──────────────

    /// Oracle `TestHandleGetDocumentContext_WithEntities`: entities linked
    /// across two chunks are de-duplicated and carry id/name/type/domain.
    #[test]
    fn doc_context_entities_deduplicated_across_chunks() {
        let db = test_util::in_memory_db();
        let doc_id = seed_document(&db, "markdown", "/docs/test.md", None);
        let chunk1 = seed_chunk(&db, doc_id, "Alice works at Acme.", 1, (None, None));
        let chunk2 = seed_chunk(&db, doc_id, "Bob manages team.", 2, (None, None));
        let alice = seed_entity(&db, "PERSON", "Alice", "hr");
        let acme = seed_entity(&db, "ORGANIZATION", "Acme Corp", "hr");
        link_chunk(&db, chunk1, alice);
        link_chunk(&db, chunk1, acme);
        link_chunk(&db, chunk2, alice);

        let response = doc_context(
            &db,
            Some(serde_json::json!({
                "document_id": doc_id.to_string(),
                "include_entities": true,
            })),
        )
        .unwrap();

        let entities = response["entities"].as_array().unwrap();
        // Alice appears in both chunks → exactly two entities.
        assert_eq!(entities.len(), 2, "{response}");
        let names: Vec<&str> = entities
            .iter()
            .map(|entity| entity["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"Alice"), "{names:?}");
        assert!(names.contains(&"Acme Corp"), "{names:?}");
        for entity in entities {
            assert!(entity["id"].is_i64(), "{entity}");
            assert!(entity["type"].is_string(), "{entity}");
            assert_eq!(entity["domain"], "hr");
        }
    }

    // ── get_document_context: facts (oracle _FactsMultipleEntitiesPerChunk)

    /// The oracle `_FactsMultipleEntitiesPerChunk` fixture: two chunks,
    /// three entities, links (Alice+Acme → chunk 1, Bob+Acme → chunk 2),
    /// three approved facts. Returns (db, doc_id, [fact ids]).
    fn seeded_facts_fixture() -> (db::Db, i64, Vec<i64>) {
        let db = test_util::in_memory_db();
        let doc_id = seed_document(&db, "markdown", "/docs/team.md", None);
        let chunk1 = seed_chunk(&db, doc_id, "Alice works at Acme.", 1, (None, None));
        let chunk2 = seed_chunk(&db, doc_id, "Bob leads team at Acme.", 2, (None, None));
        let alice = seed_entity(&db, "PERSON", "Alice", "hr");
        let bob = seed_entity(&db, "PERSON", "Bob", "hr");
        let acme = seed_entity(&db, "ORGANIZATION", "Acme Corp", "hr");
        link_chunk(&db, chunk1, alice);
        link_chunk(&db, chunk1, acme);
        link_chunk(&db, chunk2, bob);
        link_chunk(&db, chunk2, acme);
        let facts = vec![
            seed_fact(&db, alice, "works_at", acme, "hr"),
            seed_fact(&db, bob, "leads", acme, "hr"),
            seed_fact(&db, alice, "reports_to", bob, "hr"),
        ];
        (db, doc_id, facts)
    }

    /// Oracle "all three fact IDs are present" + "no duplicate fact IDs"
    /// (include_facts=true, include_entities=false, chunks loaded).
    #[test]
    fn doc_context_fact_ids_all_present_without_duplicates() {
        let (db, doc_id, facts) = seeded_facts_fixture();

        let response = doc_context(
            &db,
            Some(serde_json::json!({
                "document_id": doc_id.to_string(),
                "include_facts": true,
                "include_entities": false,
            })),
        )
        .unwrap();

        let fact_ids: Vec<i64> = response["fact_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_i64().unwrap())
            .collect();
        assert_eq!(fact_ids.len(), 3, "{response}");
        for fact_id in &facts {
            assert!(fact_ids.contains(fact_id), "{fact_id} missing");
        }
        let unique: HashSet<i64> = fact_ids.iter().copied().collect();
        assert_eq!(unique.len(), fact_ids.len(), "duplicates: {fact_ids:?}");
        // include_entities=false → the entities array is omitted.
        assert!(response.get("entities").is_none(), "{response}");
    }

    /// Oracle `_FactsWithoutChunks`: fact ids resolve through
    /// `GetEntityIDsByDocID` when chunks are not loaded.
    #[test]
    fn doc_context_fact_ids_without_chunks() {
        let (db, doc_id, facts) = seeded_facts_fixture();

        let response = doc_context(
            &db,
            Some(serde_json::json!({
                "document_id": doc_id.to_string(),
                "include_chunks": false,
                "include_entities": false,
                "include_facts": true,
            })),
        )
        .unwrap();

        assert_eq!(response["chunk_count"], serde_json::json!(2));
        assert!(response.get("chunks").is_none(), "{response}");
        let fact_ids: Vec<i64> = response["fact_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_i64().unwrap())
            .collect();
        for fact_id in &facts {
            assert!(fact_ids.contains(fact_id), "{fact_id} missing");
        }
    }

    /// Oracle `_EntitiesWithoutChunks`: entities resolve through
    /// `GetEntityIDsByDocID` when chunks are not loaded.
    #[test]
    fn doc_context_entities_without_chunks() {
        let db = test_util::in_memory_db();
        let doc_id = seed_document(&db, "markdown", "/docs/test.md", None);
        let chunk1 = seed_chunk(&db, doc_id, "Alice works at Acme.", 1, (None, None));
        let chunk2 = seed_chunk(&db, doc_id, "Bob manages team.", 2, (None, None));
        let alice = seed_entity(&db, "PERSON", "Alice", "hr");
        let acme = seed_entity(&db, "ORGANIZATION", "Acme Corp", "hr");
        link_chunk(&db, chunk1, alice);
        link_chunk(&db, chunk1, acme);
        link_chunk(&db, chunk2, alice);

        let response = doc_context(
            &db,
            Some(serde_json::json!({
                "document_id": doc_id.to_string(),
                "include_chunks": false,
                "include_entities": true,
            })),
        )
        .unwrap();

        assert!(response.get("chunks").is_none(), "{response}");
        assert_eq!(response["chunk_count"], serde_json::json!(2));
        assert_eq!(
            response["entities"].as_array().unwrap().len(),
            2,
            "{response}"
        );
    }

    /// Oracle `_EntitiesAndFactsWithoutChunks`: both sections at once, with
    /// `include_chunks=false`.
    #[test]
    fn doc_context_entities_and_facts_without_chunks() {
        let (db, doc_id, facts) = seeded_facts_fixture();

        let response = doc_context(
            &db,
            Some(serde_json::json!({
                "document_id": doc_id.to_string(),
                "include_chunks": false,
                "include_entities": true,
                "include_facts": true,
            })),
        )
        .unwrap();

        assert!(response.get("chunks").is_none(), "{response}");
        assert_eq!(
            response["entities"].as_array().unwrap().len(),
            3,
            "{response}"
        );
        let fact_ids: Vec<i64> = response["fact_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_i64().unwrap())
            .collect();
        assert!(fact_ids.contains(&facts[0]), "{fact_ids:?}");
    }

    /// Non-approved facts never appear in `fact_ids` (frozen schema: "from
    /// approved facts" — the db DAO is approved-only; a pending fact must
    /// not leak).
    #[test]
    fn doc_context_fact_ids_approved_only() {
        let (db, doc_id, facts) = seeded_facts_fixture();
        // Promote the third fact to pending (raw SQL; the DAO only creates
        // approved facts — same pattern as tools::facts).
        db.with_conn(|conn| {
            conn.execute(
                "UPDATE facts SET status = 'pending' WHERE id = ?",
                [facts[2]],
            )
        })
        .expect("checkout")
        .expect("status update");

        let response = doc_context(
            &db,
            Some(serde_json::json!({
                "document_id": doc_id.to_string(),
                "include_facts": true,
                "include_entities": false,
            })),
        )
        .unwrap();

        let fact_ids: Vec<i64> = response["fact_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_i64().unwrap())
            .collect();
        assert_eq!(fact_ids.len(), 2, "{response}");
        assert!(!fact_ids.contains(&facts[2]), "pending fact leaked");
    }

    // ── get_chunk_by_id: argument validation ───────────────────────────────

    /// Oracle "empty chunk id returns error" + "missing chunk id returns
    /// error".
    #[test]
    fn chunk_id_missing_chunk_id_is_an_error() {
        let db = test_util::in_memory_db();
        for args in [
            None,
            Some(serde_json::json!({})),
            Some(serde_json::json!({ "chunk_id": "" })),
        ] {
            let err = chunk_by_id(&db, args).unwrap_err();
            assert!(
                matches!(err, McpError::InvalidArguments { .. }),
                "got: {err:?}"
            );
            assert!(
                err.to_string().contains("'chunk_id' argument is required"),
                "got: {err}"
            );
        }
    }

    /// Oracle "non-integer chunk id returns error".
    #[test]
    fn chunk_id_non_integer_chunk_id_is_an_error() {
        let db = test_util::in_memory_db();
        for raw in ["abc", "1.5", " 1"] {
            let err = chunk_by_id(&db, Some(serde_json::json!({ "chunk_id": raw }))).unwrap_err();
            assert!(
                matches!(err, McpError::InvalidArguments { .. }),
                "got: {err:?}"
            );
            assert!(err.to_string().contains("must be an integer"), "got: {err}");
        }
    }

    // ── get_chunk_by_id: not-found ─────────────────────────────────────────

    /// Oracle "nonexistent chunk id returns error": not-found is a tool
    /// error (recorded deviation: the message says "id", not the oracle's
    /// "Predicate").
    #[test]
    fn chunk_id_nonexistent_chunk_is_not_found() {
        let db = test_util::in_memory_db();
        let err = chunk_by_id(&db, Some(serde_json::json!({ "chunk_id": "99999" }))).unwrap_err();
        assert!(matches!(err, McpError::NotFound { .. }), "got: {err:?}");
        assert!(
            err.to_string().contains("chunk with id 99999 not found"),
            "got: {err}"
        );
        let result = err.into_tool_result();
        let rmcp::model::CallToolResponse::Complete(call) = result else {
            panic!("expected a complete tool result");
        };
        assert_eq!(call.is_error, Some(true));
    }

    // ── get_chunk_by_id: full structure (oracle _ResponseFields) ───────────

    /// Oracle `TestHandleGetChunkByID_ResponseFields`: offsets, sequence
    /// number, document brief; plus the linked entity (oracle "valid chunk
    /// returns data with document and entities").
    #[test]
    fn chunk_id_response_fields() {
        let db = test_util::in_memory_db();
        let doc_id = seed_document(&db, "json", "/data/test.json", None);
        let chunk_id = seed_chunk(&db, doc_id, "Test chunk content.", 3, (Some(10), Some(250)));
        let alice = seed_entity(&db, "PERSON", "Alice", "hr");
        link_chunk(&db, chunk_id, alice);

        let response = chunk_by_id(
            &db,
            Some(serde_json::json!({ "chunk_id": chunk_id.to_string() })),
        )
        .unwrap();

        assert_eq!(response["chunk"]["id"], chunk_id);
        assert_eq!(response["chunk"]["doc_id"], doc_id);
        assert_eq!(response["chunk"]["chunk_text"], "Test chunk content.");
        assert_eq!(response["chunk"]["sequence_num"], serde_json::json!(3));
        assert_eq!(response["chunk"]["start_offset"], serde_json::json!(10));
        assert_eq!(response["chunk"]["end_offset"], serde_json::json!(250));
        assert!(response["chunk"]["created_at"].is_string());

        assert_eq!(response["document"]["id"], doc_id);
        assert_eq!(response["document"]["source_type"], "json");
        assert_eq!(response["document"]["original_path"], "/data/test.json");

        let entities = response["entities"].as_array().unwrap();
        assert_eq!(entities.len(), 1, "{response}");
        assert_eq!(entities[0]["id"], alice);
        assert_eq!(entities[0]["name"], "Alice");
        assert_eq!(entities[0]["type"], "PERSON");
        assert_eq!(entities[0]["domain"], "hr");
    }

    /// A chunk without offsets omits the offset fields (oracle omitempty on
    /// `ChunkInfo` — the counterpart of `ChunkWithContext`'s always-present
    /// offsets in the document context).
    #[test]
    fn chunk_id_without_offsets_omits_offset_fields() {
        let db = test_util::in_memory_db();
        let doc_id = seed_document(&db, "markdown", "/docs/test.md", None);
        let chunk_id = seed_chunk(&db, doc_id, "Alice works at Acme Corp.", 1, (None, None));

        let response = chunk_by_id(
            &db,
            Some(serde_json::json!({ "chunk_id": chunk_id.to_string() })),
        )
        .unwrap();

        assert!(
            response["chunk"].get("start_offset").is_none(),
            "{response}"
        );
        assert!(response["chunk"].get("end_offset").is_none(), "{response}");
        // No linked entities → the field is omitted.
        assert!(response.get("entities").is_none(), "{response}");
    }
}
