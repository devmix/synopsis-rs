//! Integration tests for the `get_document_context` and `get_chunk_by_id`
//! tools (extracted from `src/tools/documents.rs` by test-hygiene-phase-1
//! task 1.4). The 19 tests previously lived in the inline `#[cfg(test)] mod
//! tests` module; they now run against the public API
//! (`mcp::tools::documents`), with the `crate::`/`super::*` imports rewritten
//! to the crate name.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;

use db::{ChunkDao, ChunkEntityDao, ConnectionOrTx, DocumentDao, EntityDao, FactDao, test_util};
use mcp::error::McpError;
use mcp::tools::documents::{handle_get_chunk_by_id, handle_get_document_context};
use serde_json::Value;

// ── fixtures ──────────────────────────────────────────────────────────────

/// A document with `metadata_json`; returns its id.
fn seed_document(db: &db::Db, source_type: &str, path: &str, metadata: Option<&str>) -> i64 {
    db.with_conn(|conn| {
        DocumentDao::new(ConnectionOrTx::Connection(conn)).create(source_type, path, metadata, None)
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

/// Empty and missing document id both return an error.
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

/// A non-integer document id returns an error.
#[test]
fn doc_context_non_integer_document_id_is_an_error() {
    let db = test_util::in_memory_db();
    for raw in ["not_a_number", "1.5", " 1"] {
        let err = doc_context(&db, Some(serde_json::json!({ "document_id": raw }))).unwrap_err();
        assert!(
            matches!(err, McpError::InvalidArguments { .. }),
            "got: {err:?}"
        );
        assert!(err.to_string().contains("must be an integer"), "got: {err}");
    }
}

// ── get_document_context: not-found ────────────────────────────────────

/// A nonexistent document id returns an error: not-found is a tool error.
#[test]
fn doc_context_nonexistent_document_is_not_found() {
    let db = test_util::in_memory_db();
    let err = doc_context(&db, Some(serde_json::json!({ "document_id": "99999" }))).unwrap_err();
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

// ── get_document_context: full structure ─────────────────────────────────

/// Full response structure: metadata with a `domain` array, three
/// chunks, the full `document` object.
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
    // Offsets are always present in the document context (0 when NULL).
    assert_eq!(chunks[0]["start_offset"], serde_json::json!(0));
    assert_eq!(chunks[0]["end_offset"], serde_json::json!(0));
    // No entities / facts seeded → the fields are omitted.
    assert!(response.get("entities").is_none(), "{response}");
    assert!(response.get("fact_ids").is_none(), "{response}");
    // No image keys in the metadata → the field is omitted.
    assert!(response.get("image_paths").is_none(), "{response}");
}

/// A bare document is a valid response with `chunk_count = 0` and no
/// optional fields.
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
/// `attachments` metadata keys: every non-empty string of every key,
/// concatenated in key order (no validation that a value looks like a
/// path).
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

/// `domains` also accepts a plain `domain` string.
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

// ── get_document_context: include_chunks=false ───────────────────────────

/// With `include_chunks=false`, the count comes from a COUNT and the
/// `chunks` array is omitted.
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

// ── get_document_context: entities ───────────────────────────────────────

/// Entities linked across two chunks are de-duplicated and carry
/// id/name/type/domain.
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

// ── get_document_context: facts ──────────────────────────────────────────

/// Facts fixture: two chunks, three entities, links (Alice+Acme → chunk
/// 1, Bob+Acme → chunk 2), three approved facts. Returns (db, doc_id,
/// [fact ids]).
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

/// All three fact IDs are present and none are duplicated
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

/// Fact ids resolve through the document's entity links when chunks are
/// not loaded.
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

/// Entities resolve through the document's entity links when chunks are
/// not loaded.
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

/// Both the entities and facts sections at once, with
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

/// Empty and missing chunk id both return an error.
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

/// A non-integer chunk id returns an error.
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

/// A nonexistent chunk id returns an error: not-found is a tool error.
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

// ── get_chunk_by_id: full structure ──────────────────────────────────────

/// Full response structure: offsets, sequence number, document brief;
/// plus the linked entity (a valid chunk returns data with document and
/// entities).
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

/// A chunk without offsets omits the offset fields — the counterpart of
/// the document context's always-present offsets.
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
