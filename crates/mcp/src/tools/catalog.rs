//! The `catalog_overview` and `catalog_documents` tools: knowledge-base
//! statistics and the paginated document listing (design D4).
//!
//! Oracle mapping: `../synopsis/internal/mcp/handlers/{catalog_overview.go,
//! catalog_documents.go}` plus the cursor helpers in
//! `../synopsis/internal/mcp/handlers/pagination.go` (ported to
//! [`crate::pagination`], task 5.2).
//!
//! Thin handlers per design D2: parse the frozen-schema arguments → db DAOs
//! → oracle-shaped JSON. No business logic lives here.
//!
//! **Recorded deviations (error text):** the oracle prefixes its tool-error
//! messages with `"Error …"`; this crate uses the [`McpError`] conventions
//! established by tasks 5.1/5.3 (e.g. `"invalid arguments for tool
//! 'catalog_documents': …"`). The tool-error structure (is_error result with
//! a text block) is the same; internal message text is not part of the
//! frozen contract.
//!
//! **Response parity:** field names, order and optionality match the Go
//! structs (`CatalogOverviewResponse`, `CatalogDocumentsResponse`), so the
//! wire JSON matches the oracle's marshal output. The count maps use
//! `BTreeMap` because Go's `json.Marshal` emits map keys sorted, while a
//! `HashMap` would not be deterministic.

use std::collections::BTreeMap;

use db::{
    ChunkDao, ConnectionOrTx, Document, DocumentDao, DocumentFilter, EntityDao, EntityFilter,
    EntityLinkDao, FactDao,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::McpError;
use crate::pagination::{DEFAULT_PAGE_SIZE, Page, decode_cursor, normalize_page_size};

/// The frozen tool name (`mcp-contract`).
pub const CATALOG_OVERVIEW: &str = "catalog_overview";
/// The frozen tool name (`mcp-contract`).
pub const CATALOG_DOCUMENTS: &str = "catalog_documents";

// ── catalog_overview ────────────────────────────────────────────────────────

/// The `catalog_overview` response (oracle `CatalogOverviewResponse`): field
/// order matches the Go struct, so the wire JSON matches the oracle's
/// marshal order.
#[derive(Debug, Serialize)]
struct OverviewResponse {
    /// Number of documents.
    document_count: i64,
    /// Number of chunks.
    chunk_count: i64,
    /// Number of entities.
    entity_count: i64,
    /// Number of facts (all statuses — the oracle's `FactDAO.Count` has no
    /// status filter; approved-only applies to the fact *listing* tools).
    fact_count: i64,
    /// Number of documents per source type.
    documents_by_type: BTreeMap<String, i64>,
    /// Number of entities per type.
    entities_by_type: BTreeMap<String, i64>,
    /// Number of entities per domain.
    entities_by_domain: BTreeMap<String, i64>,
    /// Distinct document domains (from `metadata_json`), sorted.
    domains: Vec<String>,
    /// Distinct entity types, sorted.
    entity_types: Vec<String>,
    /// Distinct entities referenced in `entity_links` (graph nodes).
    graph_node_count: i64,
    /// Number of `entity_links` rows (graph edges).
    graph_edge_count: i64,
}

/// Handle the `catalog_overview` tool call (design D2/D4).
///
/// The frozen schema takes no parameters; the oracle ignored its request
/// arguments too, so `args` is unused.
pub fn handle_catalog_overview(db: &db::Db, _args: Option<&Value>) -> Result<Value, McpError> {
    let overview = db
        .with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            let documents = DocumentDao::new(exec);
            let chunks = ChunkDao::new(exec);
            let entities = EntityDao::new(exec);
            let facts = FactDao::new(exec);
            let links = EntityLinkDao::new(exec);
            Ok(OverviewResponse {
                document_count: documents.count(&DocumentFilter::default())?,
                chunk_count: chunks.count()?,
                entity_count: entities.count(&EntityFilter::default())?,
                fact_count: facts.count()?,
                documents_by_type: documents.documents_by_type()?.into_iter().collect(),
                entities_by_type: entities.types_by_count()?.into_iter().collect(),
                entities_by_domain: entities.domains_by_count()?.into_iter().collect(),
                domains: documents.unique_domains()?,
                entity_types: entities.unique_types()?,
                graph_node_count: links.graph_node_count()?,
                graph_edge_count: links.count()?,
            })
        })
        .and_then(|result| result)?;
    to_value(CATALOG_OVERVIEW, overview)
}

// ── catalog_documents ───────────────────────────────────────────────────────

/// Parsed `catalog_documents` arguments (frozen schema: `page_size`,
/// `cursor`, `domain`, `source_type`, `name`). Unknown keys are ignored, as
/// in the oracle's `req.Get*` accessors.
#[derive(Debug, Deserialize)]
struct CatalogDocumentsArgs {
    /// Number of items per page (number, default 20, range 1-200).
    page_size: Option<Value>,
    /// Opaque cursor (empty/omitted = first page).
    cursor: Option<String>,
    /// Optional domain filter (empty = all domains).
    domain: Option<String>,
    /// Optional source-type filter (empty = all types).
    source_type: Option<String>,
    /// Optional name substring filter on `original_path` (empty = no filter).
    name: Option<String>,
}

/// A document entry (oracle `CatalogDocument`): field order matches the Go
/// struct.
#[derive(Debug, Serialize)]
struct CatalogDocument {
    /// Document row id.
    id: i64,
    /// Originating source type.
    source_type: String,
    /// Path of the original file.
    original_path: String,
    /// Document domains from `metadata_json` `$.domain` (string or array);
    /// always an array, empty when the metadata has no domain.
    domain: Vec<String>,
    /// The parsed `metadata_json`; the raw string when it is not valid JSON
    /// (the oracle's fallback); absent when there is no metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<Value>,
    /// Creation timestamp.
    created_at: String,
    /// Last-update timestamp.
    updated_at: String,
}

/// The `catalog_documents` response (oracle `CatalogDocumentsResponse`).
#[derive(Debug, Serialize)]
struct DocumentsResponse {
    /// The page of documents (empty, not null, when there are no matches).
    documents: Vec<CatalogDocument>,
    /// Total number of matching documents (before pagination).
    total_count: i64,
    /// Cursor for the next page; present only when more rows exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

/// Handle the `catalog_documents` tool call (design D2/D4).
///
/// `args` is the raw JSON argument object (`None` = no arguments). The
/// result is the oracle-shaped payload the server serializes into the tool
/// response's text block.
pub fn handle_catalog_documents(db: &db::Db, args: Option<&Value>) -> Result<Value, McpError> {
    let args = parse_args(args)?;

    let limit = parse_page_size(args.page_size.as_ref());
    let cursor = args.cursor.unwrap_or_default();
    // Oracle parity: an EMPTY cursor starts the first page with the
    // REQUESTED page size; only a non-empty cursor overrides the offset AND
    // limit (the cursor carries its own page window).
    let page = if cursor.is_empty() {
        Page::first(limit)
    } else {
        decode_cursor(&cursor).map_err(|err| McpError::InvalidArguments {
            tool: CATALOG_DOCUMENTS,
            reason: format!("invalid cursor: {err}"),
        })?
    };

    let filter = DocumentFilter {
        domain: args.domain.filter(|domain| !domain.is_empty()),
        source_type: args
            .source_type
            .filter(|source_type| !source_type.is_empty()),
        name: args.name.filter(|name| !name.is_empty()),
    };
    let (documents, total_count) = db
        .with_conn(|conn| {
            let documents = DocumentDao::new(ConnectionOrTx::Connection(conn));
            documents.list_paginated(page.offset, page.limit, &filter)
        })
        .and_then(|result| result)?;

    let response = DocumentsResponse {
        documents: documents.iter().map(catalog_document).collect(),
        total_count,
        next_cursor: page.next_cursor(total_count),
    };
    to_value(CATALOG_DOCUMENTS, response)
}

/// Deserialize the argument object; `None` (no arguments) is the empty
/// object, so every filter is absent rather than a parse failure.
fn parse_args(args: Option<&Value>) -> Result<CatalogDocumentsArgs, McpError> {
    let value = args
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));
    serde_json::from_value(value).map_err(|err| McpError::InvalidArguments {
        tool: CATALOG_DOCUMENTS,
        reason: err.to_string(),
    })
}

/// Parse `page_size` (frozen schema: number, default 20, range 1-200).
/// Mirrors the oracle's `req.GetInt` leniency: missing or unparseable values
/// fall back to the default rather than erroring; floats truncate toward
/// zero like Go's `int(v)` conversion. Out-of-range values are clamped by
/// [`normalize_page_size`] (oracle `NormalizePageSize`), not rejected.
fn parse_page_size(value: Option<&Value>) -> i64 {
    let Some(size) = value.and_then(|value| match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|f| f as i64)),
        Value::String(text) => text.parse::<i64>().ok(),
        _ => None,
    }) else {
        return DEFAULT_PAGE_SIZE;
    };
    normalize_page_size(size)
}

/// Map a stored document to the wire entry (oracle field mapping in
/// `handlers/catalog_documents.go`).
fn catalog_document(doc: &Document) -> CatalogDocument {
    let (domain, metadata) = metadata_fields(doc.metadata_json.as_deref());
    CatalogDocument {
        id: doc.id,
        source_type: doc.source_type.clone(),
        original_path: doc.original_path.clone(),
        domain,
        metadata,
        created_at: doc.created_at.clone(),
        updated_at: doc.updated_at.clone(),
    }
}

/// Extract the `$.domain` value (string or array of strings) and the parsed
/// metadata from a document's `metadata_json` (oracle field mapping in
/// `handlers/catalog_documents.go`). No/empty metadata → no domains, no
/// metadata field. Malformed JSON → no domains, the raw string as the
/// metadata (the oracle's fallback). A JSON `null` → no metadata field
/// (the oracle's `interface{}` unmarshals `null` to a nil value, which
/// `omitempty` drops).
fn metadata_fields(metadata_json: Option<&str>) -> (Vec<String>, Option<Value>) {
    let Some(raw) = metadata_json.filter(|raw| !raw.is_empty()) else {
        return (Vec::new(), None);
    };
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => (extract_domains(&value), (!value.is_null()).then_some(value)),
        Err(_) => (Vec::new(), Some(Value::String(raw.to_owned()))),
    }
}

/// The `$.domain` members as strings: a non-empty string wraps into a
/// one-element array; an array keeps its non-empty string members (non-string
/// items are skipped, as in the oracle's type switch).
fn extract_domains(metadata: &Value) -> Vec<String> {
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

/// Serialize the response payload; a failure here is unreachable (all fields
/// are primitives), but the no-panic rule (design D7) keeps an error path
/// instead of a panic (same convention as `tools::search`).
fn to_value<T: Serialize>(tool: &'static str, response: T) -> Result<Value, McpError> {
    serde_json::to_value(response).map_err(|err| McpError::InvalidArguments {
        tool,
        reason: format!("serializing the response failed: {err}"),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use db::{EntityLink, EntityLinkDao, test_util};

    use super::*;

    /// The oracle `TestHandleCatalogOverview` fixture: 2 documents (markdown
    /// + json), 3 chunks, 2 entities, 1 fact, 1 entity link.
    fn seeded_overview_db() -> db::Db {
        let db = test_util::in_memory_db();
        db.exec_tx(|tx| -> Result<(), db::DbError> {
            let exec = ConnectionOrTx::Transaction(&*tx);
            let documents = DocumentDao::new(exec);
            let chunks = ChunkDao::new(exec);
            let entities = EntityDao::new(exec);
            let facts = FactDao::new(exec);
            let links = EntityLinkDao::new(exec);

            let doc1 = documents.create(
                "markdown",
                "/docs/hr.md",
                Some(r#"{"author":"alice","domain":["hr"]}"#),
                None,
            )?;
            let doc2 = documents.create(
                "json",
                "/data/engineering.json",
                Some(r#"{"domain":["engineering"]}"#),
                None,
            )?;

            chunks.create(doc1, "HR policy text.", 1, None, None)?;
            chunks.create(doc1, "More HR text.", 2, None, None)?;
            chunks.create(doc2, "Engineering specs.", 1, None, None)?;

            let ent1 = entities.create(
                "employee",
                "Alice",
                "hr",
                Some("Senior engineer"),
                None,
                None,
            )?;
            let ent2 = entities.create("policy", "NDA", "engineering", None, None, None)?;

            facts.create(Some(ent1), "owns", Some(ent2), "hr", None, None, None)?;

            links.create(&EntityLink {
                subject_entity_id: ent1,
                target_entity_id: ent2,
                relation_type: "same_entity".into(),
                method: "rule".into(),
                confidence: 0.95,
                evidence: None,
            })?;
            Ok(())
        })
        .expect("seed transaction commits");
        db
    }

    /// The oracle `TestHandleCatalogDocuments` fixture: 2 documents, one with
    /// array domains `["hr","policy"]`, one with `["engineering"]`.
    fn seeded_documents_db() -> db::Db {
        let db = test_util::in_memory_db();
        db.exec_tx(|tx| -> Result<(), db::DbError> {
            let documents = DocumentDao::new(ConnectionOrTx::Transaction(&*tx));
            documents.create(
                "markdown",
                "/docs/hr_policy.md",
                Some(r#"{"author":"alice","domain":["hr","policy"]}"#),
                None,
            )?;
            documents.create(
                "json",
                "/data/engineering.json",
                Some(r#"{"author":"bob","domain":["engineering"]}"#),
                None,
            )?;
            Ok(())
        })
        .expect("seed transaction commits");
        db
    }

    /// `n` documents without metadata (the oracle cursor-pagination fixture).
    fn seeded_n_documents_db(n: usize) -> db::Db {
        let db = test_util::in_memory_db();
        db.exec_tx(|tx| -> Result<(), db::DbError> {
            let documents = DocumentDao::new(ConnectionOrTx::Transaction(&*tx));
            for i in 0..n {
                documents.create(
                    "markdown",
                    &format!("/docs/doc_{}.md", char::from(b'A' + i as u8)),
                    None,
                    None,
                )?;
            }
            Ok(())
        })
        .expect("seed transaction commits");
        db
    }

    fn overview(db: &db::Db) -> Value {
        handle_catalog_overview(db, None).expect("overview must not fail")
    }

    fn documents(db: &db::Db, args: Option<Value>) -> Result<Value, McpError> {
        handle_catalog_documents(db, args.as_ref())
    }

    // ── catalog_overview (oracle catalog_overview_test.go cases) ──────────

    #[test]
    fn overview_counts_match_seeded_db() {
        let response = overview(&seeded_overview_db());

        assert_eq!(response["document_count"], serde_json::json!(2));
        assert_eq!(response["chunk_count"], serde_json::json!(3));
        assert_eq!(response["entity_count"], serde_json::json!(2));
        assert_eq!(response["fact_count"], serde_json::json!(1));

        assert_eq!(
            response["documents_by_type"]["markdown"],
            serde_json::json!(1)
        );
        assert_eq!(response["documents_by_type"]["json"], serde_json::json!(1));
        assert_eq!(
            response["entities_by_type"]["employee"],
            serde_json::json!(1)
        );
        assert_eq!(response["entities_by_type"]["policy"], serde_json::json!(1));
        assert_eq!(response["entities_by_domain"]["hr"], serde_json::json!(1));
        assert_eq!(
            response["entities_by_domain"]["engineering"],
            serde_json::json!(1)
        );

        // Domains come from the document metadata (both array members).
        let domains: Vec<&str> = response["domains"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(domains, ["engineering", "hr"]);

        let entity_types: Vec<&str> = response["entity_types"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(entity_types, ["employee", "policy"]);

        assert_eq!(response["graph_node_count"], serde_json::json!(2));
        assert_eq!(response["graph_edge_count"], serde_json::json!(1));
    }

    /// Oracle `TestHandleCatalogOverview_EmptyDB`: all counters are zero.
    #[test]
    fn overview_empty_db_is_all_zeros() {
        let response = overview(&test_util::in_memory_db());

        for key in [
            "document_count",
            "chunk_count",
            "entity_count",
            "fact_count",
            "graph_node_count",
            "graph_edge_count",
        ] {
            assert_eq!(response[key], serde_json::json!(0), "{key}");
        }
        assert_eq!(response["documents_by_type"], serde_json::json!({}));
        assert_eq!(response["entities_by_type"], serde_json::json!({}));
        assert_eq!(response["entities_by_domain"], serde_json::json!({}));
    }

    /// Oracle `TestHandleCatalogOverview_EmptyDB_ReturnsEmptyArrays`: the
    /// list fields serialize as `[]`, not `null`.
    #[test]
    fn overview_empty_db_lists_are_empty_arrays_not_null() {
        let wire = serde_json::to_string(&overview(&test_util::in_memory_db())).unwrap();
        assert!(wire.contains("\"domains\":[]"), "got: {wire}");
        assert!(wire.contains("\"entity_types\":[]"), "got: {wire}");
    }

    /// Oracle `TestHandleCatalogOverview_MultiEdgePath`: the graph counts
    /// union the endpoints of a multi-edge chain (1→2, 2→3 → 3 nodes, 2
    /// edges) — the oracle's regression case for its broken intersection
    /// formula, which the db crate's `graph_node_count` (distinct subjects ∪
    /// targets) handles correctly by construction.
    #[test]
    fn overview_multi_edge_path_graph_counts() {
        let db = test_util::in_memory_db();
        db.exec_tx(|tx| -> Result<(), db::DbError> {
            let exec = ConnectionOrTx::Transaction(&*tx);
            let entities = EntityDao::new(exec);
            let links = EntityLinkDao::new(exec);
            let a = entities.create("employee", "Node1", "test", None, None, None)?;
            let b = entities.create("employee", "Node2", "test", None, None, None)?;
            let c = entities.create("employee", "Node3", "test", None, None, None)?;
            for (subject, target) in [(a, b), (b, c)] {
                links.create(&EntityLink {
                    subject_entity_id: subject,
                    target_entity_id: target,
                    relation_type: "related".into(),
                    method: "rule".into(),
                    confidence: 0.9,
                    evidence: None,
                })?;
            }
            Ok(())
        })
        .expect("seed transaction commits");

        let response = overview(&db);
        assert_eq!(response["graph_node_count"], serde_json::json!(3));
        assert_eq!(response["graph_edge_count"], serde_json::json!(2));
    }

    /// Oracle `TestHandleCatalogOverview_DomainsFromMetadata`: domains are
    /// read from `metadata_json` for multi-domain documents.
    #[test]
    fn overview_domains_from_metadata() {
        let db = test_util::in_memory_db();
        db.with_conn(|conn| {
            let documents = DocumentDao::new(ConnectionOrTx::Connection(conn));
            documents
                .create(
                    "markdown",
                    "/docs/multi-domain.md",
                    Some(r#"{"domain":["product","hr","engineering"]}"#),
                    None,
                )
                .unwrap()
        })
        .unwrap();

        let response = overview(&db);
        let domains: Vec<&str> = response["domains"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(domains, ["engineering", "hr", "product"]);
    }

    // ── catalog_documents: filters (oracle TestHandleCatalogDocuments) ────

    #[test]
    fn documents_empty_request_returns_all() {
        let response = documents(&seeded_documents_db(), None).unwrap();
        assert_eq!(response["total_count"], serde_json::json!(2));
        assert_eq!(response["documents"].as_array().unwrap().len(), 2);
        assert!(
            response.get("next_cursor").is_none(),
            "all rows fit one page: {response}"
        );
    }

    #[test]
    fn documents_page_size_limits_results_and_emits_next_cursor() {
        let response = documents(
            &seeded_documents_db(),
            Some(serde_json::json!({ "page_size": 1 })),
        )
        .unwrap();
        assert_eq!(response["documents"].as_array().unwrap().len(), 1);
        assert_eq!(response["total_count"], serde_json::json!(2));
        assert!(response["next_cursor"].is_string(), "{response}");
    }

    #[test]
    fn documents_domain_filter() {
        let response = documents(
            &seeded_documents_db(),
            Some(serde_json::json!({ "domain": "hr" })),
        )
        .unwrap();
        assert_eq!(response["total_count"], serde_json::json!(1));
        assert_eq!(
            response["documents"][0]["original_path"],
            "/docs/hr_policy.md"
        );
    }

    #[test]
    fn documents_source_type_filter() {
        let response = documents(
            &seeded_documents_db(),
            Some(serde_json::json!({ "source_type": "json" })),
        )
        .unwrap();
        assert_eq!(response["total_count"], serde_json::json!(1));
        assert_eq!(
            response["documents"][0]["original_path"],
            "/data/engineering.json"
        );
    }

    /// Name substring filter (frozen schema `name` → the oracle's
    /// `ListPaginatedWithName`): case-insensitive on `original_path`.
    #[test]
    fn documents_name_filter_is_case_insensitive_substring() {
        let response = documents(
            &seeded_documents_db(),
            Some(serde_json::json!({ "name": "HR_POLICY" })),
        )
        .unwrap();
        assert_eq!(response["total_count"], serde_json::json!(1));
        assert_eq!(
            response["documents"][0]["original_path"],
            "/docs/hr_policy.md"
        );

        let none = documents(
            &seeded_documents_db(),
            Some(serde_json::json!({ "name": "nonexistent" })),
        )
        .unwrap();
        assert_eq!(none["total_count"], serde_json::json!(0));
        assert_eq!(none["documents"], serde_json::json!([]));
    }

    // ── catalog_documents: cursor pagination (oracle _CursorPagination) ───

    #[test]
    fn documents_cursor_pagination_walk() {
        let db = seeded_n_documents_db(5);

        // First page.
        let first = documents(&db, Some(serde_json::json!({ "page_size": 2 }))).unwrap();
        assert_eq!(first["documents"].as_array().unwrap().len(), 2);
        assert_eq!(first["total_count"], serde_json::json!(5));
        assert!(first["next_cursor"].is_string(), "{first}");

        // Second page (via the cursor; it carries its own page window).
        let second = documents(
            &db,
            Some(serde_json::json!({ "cursor": first["next_cursor"].as_str().unwrap() })),
        )
        .unwrap();
        assert_eq!(second["documents"].as_array().unwrap().len(), 2);

        // No overlap between pages.
        let ids = |page: &Value| {
            page["documents"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d["id"].as_i64().unwrap())
                .collect::<std::collections::HashSet<_>>()
        };
        assert!(
            ids(&first).is_disjoint(&ids(&second)),
            "pages must not overlap"
        );

        // Third (last) page.
        let third = documents(
            &db,
            Some(serde_json::json!({ "cursor": second["next_cursor"].as_str().unwrap() })),
        )
        .unwrap();
        assert_eq!(third["documents"].as_array().unwrap().len(), 1);
        assert!(
            third.get("next_cursor").is_none(),
            "no next_cursor on the last page: {third}"
        );
    }

    /// An empty cursor honours the REQUESTED page size (the cursor's own
    /// window only applies to a non-empty cursor — oracle control flow).
    #[test]
    fn documents_empty_cursor_uses_requested_page_size() {
        let db = seeded_n_documents_db(5);
        let response = documents(
            &db,
            Some(serde_json::json!({ "page_size": 3, "cursor": "" })),
        )
        .unwrap();
        assert_eq!(response["documents"].as_array().unwrap().len(), 3);
        assert!(response["next_cursor"].is_string(), "{response}");
    }

    /// Oracle `TestHandleCatalogDocuments_InvalidCursor`.
    #[test]
    fn documents_invalid_cursor_is_an_error() {
        let db = test_util::in_memory_db();
        let err = documents(
            &db,
            Some(serde_json::json!({ "cursor": "not-valid-base64!!!" })),
        )
        .unwrap_err();
        assert!(
            matches!(err, McpError::InvalidArguments { .. }),
            "got: {err:?}"
        );
        assert!(err.to_string().contains("invalid cursor"), "got: {err}");
    }

    /// A valid base64 cursor with a malformed payload is also rejected.
    #[test]
    fn documents_malformed_cursor_payload_is_an_error() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD;

        let db = test_util::in_memory_db();
        let cursor = STANDARD.encode(b"[1,2,3]");
        let err = documents(&db, Some(serde_json::json!({ "cursor": cursor }))).unwrap_err();
        assert!(
            matches!(err, McpError::InvalidArguments { .. }),
            "got: {err:?}"
        );
    }

    /// `page_size` out of range (or unparseable) clamps to the default 20
    /// (oracle `NormalizePageSize`) instead of erroring.
    #[test]
    fn documents_page_size_out_of_range_defaults_to_twenty() {
        let db = seeded_n_documents_db(25);
        for page_size in [
            serde_json::json!(0),
            serde_json::json!(500),
            serde_json::json!("x"),
        ] {
            let response =
                documents(&db, Some(serde_json::json!({ "page_size": page_size }))).unwrap();
            assert_eq!(
                response["documents"].as_array().unwrap().len(),
                20,
                "page_size {page_size} must clamp to the default"
            );
            assert!(response["next_cursor"].is_string(), "{response}");
        }
    }

    // ── catalog_documents: shapes (oracle _EmptyDB + field mapping) ───────

    #[test]
    fn documents_empty_db_is_empty_page_without_cursor() {
        let response = documents(&test_util::in_memory_db(), None).unwrap();
        assert_eq!(response["total_count"], serde_json::json!(0));
        assert_eq!(response["documents"], serde_json::json!([]));
        assert!(response.get("next_cursor").is_none(), "{response}");
    }

    /// Oracle field mapping for `domain` and `metadata`: array and scalar
    /// `$.domain`, no metadata, and malformed metadata (raw string fallback).
    #[test]
    fn documents_metadata_shapes() {
        let db = test_util::in_memory_db();
        db.exec_tx(|tx| -> Result<(), db::DbError> {
            let documents = DocumentDao::new(ConnectionOrTx::Transaction(&*tx));
            documents.create(
                "markdown",
                "/docs/array-domain.md",
                Some(r#"{"domain":["hr","policy"],"author":"alice"}"#),
                None,
            )?;
            documents.create(
                "json",
                "/data/scalar-domain.json",
                Some(r#"{"domain":"product"}"#),
                None,
            )?;
            documents.create("markdown", "/docs/no-metadata.md", None, None)?;
            documents.create("markdown", "/docs/bad-metadata.md", Some("not-json{"), None)?;
            Ok(())
        })
        .expect("seed transaction commits");

        let response = documents(&db, None).unwrap();
        let by_path: std::collections::HashMap<String, Value> = response["documents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| (d["original_path"].as_str().unwrap().to_owned(), d.clone()))
            .collect();

        let array = &by_path["/docs/array-domain.md"];
        assert_eq!(array["domain"], serde_json::json!(["hr", "policy"]));
        assert_eq!(
            array["metadata"]["domain"],
            serde_json::json!(["hr", "policy"])
        );
        assert_eq!(array["metadata"]["author"], "alice");

        let scalar = &by_path["/data/scalar-domain.json"];
        assert_eq!(scalar["domain"], serde_json::json!(["product"]));

        let bare = &by_path["/docs/no-metadata.md"];
        assert_eq!(bare["domain"], serde_json::json!([]));
        assert!(
            bare.get("metadata").is_none(),
            "no metadata_json → no metadata field: {bare}"
        );

        let bad = &by_path["/docs/bad-metadata.md"];
        assert_eq!(bad["domain"], serde_json::json!([]));
        assert_eq!(bad["metadata"], "not-json{");

        // Always-present fields stay present.
        for doc in by_path.values() {
            assert!(doc.get("id").is_some());
            assert!(doc.get("source_type").is_some());
            assert!(doc.get("created_at").is_some());
            assert!(doc.get("updated_at").is_some());
        }
    }
}
