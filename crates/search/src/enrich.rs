//! Result enrichment: document metadata and chunk entities (design D6).
//!
//! Flow: collect the unique document ids of the pool → one batched
//! [`db::DocumentDao::get_by_ids`] → collect the unique chunk ids → one
//! batched [`db::ChunkEntityDao::get_entities_by_chunks`] → per result:
//! `document_path`, merged `source_type` (`"lexical+pdf"` style),
//! `metadata["document_source_type"]`, `updated_at` normalized to RFC3339,
//! the raw `document_metadata_json`, `domains` (via the shared
//! `crate::document_domains`), the reranker flags
//! (`is_deprecated`/`is_official`/`valid_to`), and the chunk entities.
//! The whole pool is returned enriched (no truncation here — that is the
//! finalize pipeline, design D5).
//!
//! **Design:**
//! - `enrich` mutates the pool in place (`&mut [SearchResult]`) instead of
//!   returning a new slice (Rust idiom);
//! - the enricher is bound to concrete DAO handles (no trait-object mocks):
//!   the batch queries are pinned by in-memory SQLite tests;
//! - chunk ids are de-duplicated before the batch entity lookup;
//! - `SearchResult::source_type` is the wire string, so the merge writes
//!   `"lexical+pdf"` directly into that field.

use std::collections::{HashMap, HashSet};

use db::{ChunkEntityDao, DocumentDao};
use utils::temporal::normalize_to_rfc3339;

use crate::{SearchError, SearchResult, document_domains};

/// Adds document-level metadata and entity information to search results.
///
/// One instance per unit of work, borrowing the caller's DAOs
/// (connection- or transaction-bound per the db crate's design D2).
pub struct Enricher<'conn> {
    documents: &'conn DocumentDao<'conn>,
    chunk_entities: &'conn ChunkEntityDao<'conn>,
}

impl<'conn> Enricher<'conn> {
    /// Bind the enricher to its collaborators.
    pub fn new(
        documents: &'conn DocumentDao<'conn>,
        chunk_entities: &'conn ChunkEntityDao<'conn>,
    ) -> Self {
        Self {
            documents,
            chunk_entities,
        }
    }

    /// Enrich every result in `results` in place.
    ///
    /// Documents and entities are fetched with one batched query each
    /// (unique ids, de-duplicated). Results whose document row is missing
    /// are left untouched (empty `document_path`, unmerged `source_type`,
    /// no document metadata keys); results whose chunk has no entity links
    /// keep an empty `entities` vec. The pool is returned whole (no
    /// truncation).
    pub fn enrich(&self, results: &mut [SearchResult]) -> Result<(), SearchError> {
        if results.is_empty() {
            return Ok(());
        }

        // Unique document ids, first-seen order (one batched query).
        let mut doc_ids: Vec<i64> = Vec::with_capacity(results.len());
        let mut seen_docs: HashSet<i64> = HashSet::with_capacity(results.len());
        for result in results.iter() {
            if seen_docs.insert(result.document_id) {
                doc_ids.push(result.document_id);
            }
        }
        let docs = self.documents.get_by_ids(&doc_ids)?;
        let doc_by_id: HashMap<i64, &db::Document> = docs.iter().map(|doc| (doc.id, doc)).collect();

        // Unique chunk ids (one batched query; the DAO de-duplicates again).
        let chunk_ids: Vec<i64> = results
            .iter()
            .map(|r| r.chunk_id)
            .collect::<HashSet<i64>>()
            .into_iter()
            .collect();
        let entities_by_chunk = self.chunk_entities.get_entities_by_chunks(&chunk_ids)?;

        for result in results.iter_mut() {
            if let Some(doc) = doc_by_id.get(&result.document_id) {
                result.document_path = doc.original_path.clone();
                result.source_type = merge_source_type(&result.source_type, &doc.source_type);
                result.metadata.insert(
                    "document_source_type".to_owned(),
                    doc.source_type.clone().into(),
                );

                // Normalize updated_at to RFC3339 so the reranker's
                // freshness boost can parse it (SQLite CURRENT_TIMESTAMP
                // yields "YYYY-MM-DD HH:MM:SS"); unparseable values skip
                // the key instead of failing enrichment.
                if let Some(updated_at) = normalize_to_rfc3339(&doc.updated_at) {
                    result
                        .metadata
                        .insert("updated_at".to_owned(), updated_at.into());
                }

                if let Some(metadata_json) = doc.metadata_json.as_deref() {
                    result
                        .metadata
                        .insert("document_metadata_json".to_owned(), metadata_json.into());

                    if let Ok(meta) = serde_json::from_str::<serde_json::Value>(metadata_json) {
                        // The key is exposed whenever the document metadata
                        // carries a `domain` entry (possibly empty).
                        if meta.get("domain").is_some() {
                            result.metadata.insert(
                                "domains".to_owned(),
                                document_domains(Some(metadata_json)).into(),
                            );
                        }
                        extract_reranker_flags(&meta, &mut result.metadata);
                    }
                }
            }

            // Attach entities from the batch lookup (absent chunk ids keep
            // the empty vec).
            if let Some(entities) = entities_by_chunk.get(&result.chunk_id) {
                result.entities = entities.clone();
            }
        }

        Ok(())
    }
}

/// Combine the search source type with the document source type:
/// `"lexical" + "pdf" → "lexical+pdf"`; an empty side yields the other.
fn merge_source_type(search_source: &str, doc_source: &str) -> String {
    match (search_source.is_empty(), doc_source.is_empty()) {
        (true, true) => String::new(),
        (true, false) => doc_source.to_owned(),
        (false, true) => search_source.to_owned(),
        (false, false) => format!("{search_source}+{doc_source}"),
    }
}

/// Copy the reranker-relevant keys (`is_deprecated`, `is_official`,
/// `valid_to`) from the parsed document metadata into the result metadata.
/// Missing or wrong-typed keys are skipped — the reranker ignores absent
/// keys (an empty `valid_to` is treated as absent).
fn extract_reranker_flags(
    meta: &serde_json::Value,
    out: &mut serde_json::Map<String, serde_json::Value>,
) {
    if let Some(serde_json::Value::Bool(deprecated)) = meta.get("is_deprecated") {
        out.insert("is_deprecated".to_owned(), (*deprecated).into());
    }
    if let Some(serde_json::Value::Bool(official)) = meta.get("is_official") {
        out.insert("is_official".to_owned(), (*official).into());
    }
    if let Some(serde_json::Value::String(valid_to)) = meta.get("valid_to")
        && !valid_to.is_empty()
    {
        out.insert("valid_to".to_owned(), valid_to.clone().into());
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use db::test_util::in_memory_db;
    use db::{ChunkDao, Db, EntityDao};

    use super::*;
    use crate::SourceType;

    /// A minimal result with the given ids and pre-enrichment source type.
    fn result(chunk_id: i64, document_id: i64, source: SourceType) -> SearchResult {
        SearchResult {
            chunk_id,
            chunk_text: String::new(),
            chunk_metadata: serde_json::Map::new(),
            document_id,
            sequence_num: 0,
            start_offset: None,
            end_offset: None,
            document_path: String::new(),
            score: 1.0,
            rank: 1,
            source_type: source.as_str().to_owned(),
            metadata: serde_json::Map::new(),
            entities: Vec::new(),
        }
    }

    /// Create a document and return its id.
    fn seed_doc(db: &Db, source_type: &str, path: &str, metadata_json: Option<&str>) -> i64 {
        db.exec_tx(|tx| {
            let docs = DocumentDao::new(db::ConnectionOrTx::Transaction(&*tx));
            docs.create(source_type, path, metadata_json, None)
        })
        .expect("seed document commits")
    }

    /// Create a chunk and return its id.
    fn seed_chunk(db: &Db, doc_id: i64, seq: i64) -> i64 {
        db.exec_tx(|tx| {
            let chunks = ChunkDao::new(db::ConnectionOrTx::Transaction(&*tx));
            chunks.create(doc_id, "chunk text", seq, None, None)
        })
        .expect("seed chunk commits")
    }

    /// Create an entity and return its id.
    fn seed_entity(db: &Db, name: &str) -> i64 {
        db.exec_tx(|tx| {
            let entities = EntityDao::new(db::ConnectionOrTx::Transaction(&*tx));
            entities.create("PERSON", name, "enrich", None, None, None)
        })
        .expect("seed entity commits")
    }

    /// Link a chunk to an entity.
    fn link(db: &Db, chunk_id: i64, entity_id: i64) {
        db.with_conn(|conn| {
            let links = ChunkEntityDao::new(db::ConnectionOrTx::Connection(conn));
            links.link(chunk_id, entity_id)
        })
        .expect("connection checkout")
        .expect("link commits");
    }

    /// Run `f` with an enricher bound to pooled connections.
    fn with_enricher<T>(db: &Db, f: impl FnOnce(&Enricher<'_>) -> T) -> T {
        db.with_conn(|conn| {
            let documents = DocumentDao::new(db::ConnectionOrTx::Connection(conn));
            let chunk_entities = ChunkEntityDao::new(db::ConnectionOrTx::Connection(conn));
            f(&Enricher::new(&documents, &chunk_entities))
        })
        .expect("connection checkout")
    }

    // Batch enrichment across multiple documents and entities: paths,
    // merged source types, metadata keys and entities all land.
    #[test]
    fn enrich_fills_document_fields_and_entities() {
        let db = in_memory_db();
        let doc_a = seed_doc(
            &db,
            "policy",
            "/docs/a.md",
            Some(r#"{"domain":"combat","is_official":true}"#),
        );
        let doc_b = seed_doc(&db, "pdf", "/docs/b.pdf", None);
        let chunk_a = seed_chunk(&db, doc_a, 0);
        let chunk_b = seed_chunk(&db, doc_b, 0);
        let entity_a = seed_entity(&db, "Alpha");
        let entity_b = seed_entity(&db, "Beta");
        link(&db, chunk_a, entity_a);
        link(&db, chunk_a, entity_b);
        link(&db, chunk_b, entity_b);

        let mut results = vec![
            result(chunk_a, doc_a, SourceType::Lexical),
            result(chunk_b, doc_b, SourceType::Semantic),
        ];

        with_enricher(&db, |enricher| {
            enricher.enrich(&mut results).unwrap();
        });

        let a = &results[0];
        assert_eq!(a.document_path, "/docs/a.md");
        assert_eq!(a.source_type, "lexical+policy");
        assert_eq!(
            a.metadata
                .get("document_source_type")
                .and_then(serde_json::Value::as_str),
            Some("policy")
        );
        assert!(
            a.metadata.contains_key("updated_at"),
            "updated_at must be normalized and present"
        );
        let updated_at = a.metadata["updated_at"].as_str().unwrap();
        assert!(
            updated_at.ends_with('Z') && updated_at.contains('T'),
            "updated_at must be RFC3339, got {updated_at}"
        );
        assert_eq!(
            a.metadata["document_metadata_json"].as_str(),
            Some(r#"{"domain":"combat","is_official":true}"#)
        );
        assert_eq!(a.metadata["is_official"], serde_json::json!(true));
        assert_eq!(a.metadata["domains"], serde_json::json!(["combat"]));
        let names: Vec<&str> = a.entities.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Alpha", "Beta"]);

        let b = &results[1];
        assert_eq!(b.document_path, "/docs/b.pdf");
        assert_eq!(b.source_type, "semantic+pdf");
        assert!(
            !b.metadata.contains_key("document_metadata_json"),
            "no metadata_json → key absent"
        );
        assert_eq!(b.entities.len(), 1);
        assert_eq!(b.entities[0].id, entity_b);
    }

    // Merge semantics: both sides, one empty side, both empty.
    #[test]
    fn merge_source_type_semantics() {
        assert_eq!(merge_source_type("lexical", "pdf"), "lexical+pdf");
        assert_eq!(merge_source_type("", "pdf"), "pdf");
        assert_eq!(merge_source_type("lexical", ""), "lexical");
        assert_eq!(merge_source_type("", ""), "");
    }

    // A document with an empty source_type leaves the search side intact.
    #[test]
    fn enrich_empty_document_source_type_keeps_search_side() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "", "/docs/empty-type.md", None);
        let chunk = seed_chunk(&db, doc, 0);
        let mut results = vec![result(chunk, doc, SourceType::Hybrid)];

        with_enricher(&db, |enricher| {
            enricher.enrich(&mut results).unwrap();
        });

        assert_eq!(results[0].source_type, "hybrid");
        assert_eq!(
            results[0].metadata["document_source_type"],
            serde_json::json!("")
        );
    }

    // Reranker flags: right types extracted (false is a value, not an
    // absence), wrong types and empty valid_to skipped.
    #[test]
    fn enrich_extracts_reranker_flags() {
        let db = in_memory_db();
        let doc_full = seed_doc(
            &db,
            "policy",
            "/docs/full.md",
            Some(
                r#"{"is_official":true,"is_deprecated":false,"valid_to":"2026-12-31T00:00:00Z","domain":"combat"}"#,
            ),
        );
        let doc_bad = seed_doc(
            &db,
            "policy",
            "/docs/bad.md",
            Some(r#"{"is_official":"yes","is_deprecated":1,"valid_to":""}"#),
        );
        let doc_empty = seed_doc(&db, "policy", "/docs/empty.md", Some("{}"));
        let chunk_full = seed_chunk(&db, doc_full, 0);
        let chunk_bad = seed_chunk(&db, doc_bad, 0);
        let chunk_empty = seed_chunk(&db, doc_empty, 0);
        let mut results = vec![
            result(chunk_full, doc_full, SourceType::Lexical),
            result(chunk_bad, doc_bad, SourceType::Lexical),
            result(chunk_empty, doc_empty, SourceType::Lexical),
        ];

        with_enricher(&db, |enricher| {
            enricher.enrich(&mut results).unwrap();
        });

        let full = &results[0].metadata;
        assert_eq!(full["is_official"], serde_json::json!(true));
        assert_eq!(full["is_deprecated"], serde_json::json!(false));
        assert_eq!(full["valid_to"], serde_json::json!("2026-12-31T00:00:00Z"));

        for (i, enriched) in results[1..].iter().enumerate() {
            for key in ["is_official", "is_deprecated", "valid_to"] {
                assert!(
                    !enriched.metadata.contains_key(key),
                    "result {}: {} must be skipped",
                    i + 1,
                    key
                );
            }
        }
    }

    // End-to-end: an
    // unparseable or empty `updated_at` on a real document row skips the
    // key instead of failing enrichment.
    #[test]
    fn enrich_invalid_updated_at_skipped() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "policy", "/docs/stale.md", None);
        let chunk = seed_chunk(&db, doc, 0);
        db.with_conn(|conn| {
            conn.execute(
                "UPDATE documents SET updated_at = 'not-a-date' WHERE id = ?1",
                [doc],
            )
        })
        .expect("connection checkout")
        .expect("updated_at mangled");
        let mut results = vec![result(chunk, doc, SourceType::Lexical)];

        with_enricher(&db, |enricher| {
            enricher.enrich(&mut results).unwrap();
        });

        assert_eq!(results[0].document_path, "/docs/stale.md");
        assert!(
            !results[0].metadata.contains_key("updated_at"),
            "unparseable updated_at must skip the key"
        );
    }

    // Domains: string and array shapes, normalization, empty string, and
    // the absent `domain` key (no `domains` key at all).
    #[test]
    fn enrich_domains_shapes() {
        let db = in_memory_db();
        let doc_string = seed_doc(&db, "policy", "/docs/string.md", Some(r#"{"domain":"HR"}"#));
        let doc_array = seed_doc(
            &db,
            "policy",
            "/docs/array.md",
            Some(r#"{"domain":["hr","engineering"]}"#),
        );
        let doc_blank = seed_doc(&db, "policy", "/docs/blank.md", Some(r#"{"domain":""}"#));
        let doc_none = seed_doc(&db, "policy", "/docs/none.md", Some(r#"{"other":1}"#));
        let mut results = [doc_string, doc_array, doc_blank, doc_none]
            .into_iter()
            .map(|doc_id| result(0, doc_id, SourceType::Lexical))
            .collect::<Vec<_>>();

        with_enricher(&db, |enricher| {
            enricher.enrich(&mut results).unwrap();
        });

        assert_eq!(results[0].metadata["domains"], serde_json::json!(["hr"]));
        assert_eq!(
            results[1].metadata["domains"],
            serde_json::json!(["hr", "engineering"])
        );
        assert_eq!(results[2].metadata["domains"], serde_json::json!([]));
        assert!(
            !results[3].metadata.contains_key("domains"),
            "no domain key → no domains key"
        );
    }

    // Missing document row and missing chunk row are tolerated: the result
    // is returned unchanged (no error, no document keys, no entities).
    #[test]
    fn enrich_tolerates_missing_document_and_chunk() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "policy", "/docs/real.md", None);
        let chunk = seed_chunk(&db, doc, 0);
        let entity = seed_entity(&db, "Gamma");
        link(&db, chunk, entity);

        let mut results = vec![
            // No document row, no chunk row, no links.
            result(999_999, 999_998, SourceType::Semantic),
            // Real chunk without links; real document.
            result(999_997, doc, SourceType::Lexical),
            // The fully linked one (control).
            result(chunk, doc, SourceType::Hybrid),
        ];

        with_enricher(&db, |enricher| {
            enricher.enrich(&mut results).unwrap();
        });

        let missing = &results[0];
        assert!(missing.document_path.is_empty());
        assert_eq!(missing.source_type, "semantic");
        assert!(missing.metadata.is_empty());
        assert!(missing.entities.is_empty());

        let unlinked = &results[1];
        assert_eq!(unlinked.document_path, "/docs/real.md");
        assert!(unlinked.entities.is_empty(), "no links → empty entities");

        assert_eq!(results[2].entities.len(), 1);
        assert_eq!(results[2].entities[0].id, entity);
    }

    // Batch enrichment over a pool wider than the distinct documents:
    // every result is enriched from the shared batched lookups (one
    // get_by_ids + one get_entities_by_chunks per pool).
    #[test]
    fn enrich_batches_multiple_documents() {
        let db = in_memory_db();
        let doc_ids: Vec<i64> = (0..5)
            .map(|i| seed_doc(&db, "policy", &format!("/docs/batch-{i}.md"), None))
            .collect();
        let chunk_ids: Vec<i64> = (0..20)
            .map(|i| seed_chunk(&db, doc_ids[i % 5], i as i64))
            .collect();
        let mut results = chunk_ids
            .iter()
            .enumerate()
            .map(|(i, &chunk_id)| result(chunk_id, doc_ids[i % 5], SourceType::Lexical))
            .collect::<Vec<_>>();

        with_enricher(&db, |enricher| {
            enricher.enrich(&mut results).unwrap();
        });

        for (i, r) in results.iter().enumerate() {
            assert_eq!(r.document_path, format!("/docs/batch-{}.md", i % 5));
            assert_eq!(r.source_type, "lexical+policy");
            assert!(r.metadata.contains_key("updated_at"));
        }
    }

    // Empty pool: no-op, no queries.
    #[test]
    fn enrich_empty_pool_is_noop() {
        let db = in_memory_db();
        let mut results: Vec<SearchResult> = Vec::new();
        with_enricher(&db, |enricher| {
            enricher.enrich(&mut results).unwrap();
        });
        assert!(results.is_empty());
    }
}
