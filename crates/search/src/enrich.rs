//! Result enrichment: document metadata and chunk entities (design D6).
//!
//! Oracle mapping: `../synopsis/internal/search/enricher.go`, re-architected
//! per the 2026-08-19 migration principles (functional copy, not a code
//! copy).
//!
//! Flow: collect the unique document ids of the pool → one batched
//! [`db::DocumentDao::get_by_ids`] → collect the unique chunk ids → one
//! batched [`db::ChunkEntityDao::get_entities_by_chunks`] → per result:
//! `document_path`, merged `source_type` (`"lexical+pdf"` style),
//! `metadata["document_source_type"]`, `updated_at` normalized to RFC3339,
//! the raw `document_metadata_json`, `domains` (via the shared
//! [`crate::document_domains`]), the reranker flags
//! (`is_deprecated`/`is_official`/`valid_to`), and the chunk entities.
//! The whole pool is returned enriched (no truncation here — that is the
//! finalize pipeline, design D5).
//!
//! **Conscious deviations from the oracle:**
//! - `enrich` mutates the pool in place (`&mut [SearchResult]`) instead of
//!   returning a new slice (Rust idiom; the oracle returned the same slice
//!   it mutated);
//! - the enricher is bound to concrete DAO handles (no Go-style interface
//!   mocks): the batch queries are pinned by in-memory SQLite tests;
//! - chunk ids are de-duplicated before the batch entity lookup (the oracle
//!   passed every result's id, duplicates included);
//! - `SearchResult::source_type` is the wire string, so the merge writes
//!   `"lexical+pdf"` into the same field the oracle wrote to.

use std::collections::{HashMap, HashSet};

use db::{ChunkEntityDao, DocumentDao};

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
                if let Some(updated_at) = normalize_updated_at(&doc.updated_at) {
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
/// keys (an empty `valid_to` is treated as absent, as in the oracle).
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

/// Convert a document's `updated_at` value to RFC3339 (design D6).
///
/// Accepts RFC3339 (`2026-08-01T12:00:00Z`, with optional fractional
/// seconds and `±HH:MM` offsets — pass-through, re-rendered canonically),
/// the SQLite `CURRENT_TIMESTAMP` layout (`"2026-08-01 12:00:00"`, UTC)
/// and the same layout with fractional seconds. Fractional seconds are
/// dropped in the output (as in the oracle's `time.Format(RFC3339)`).
/// Returns `None` for empty or unparseable inputs so the caller can skip
/// the key instead of failing enrichment.
fn normalize_updated_at(value: &str) -> Option<String> {
    let b = value.as_bytes();
    if b.len() < 19 {
        return None;
    }
    // "YYYY-MM-DD" + separator + "HH:MM:SS"
    let digits = |start: usize, len: usize| b[start..start + len].iter().all(u8::is_ascii_digit);
    if b[4] != b'-'
        || b[7] != b'-'
        || b[13] != b':'
        || b[16] != b':'
        || !(digits(0, 4)
            && digits(5, 2)
            && digits(8, 2)
            && digits(11, 2)
            && digits(14, 2)
            && digits(17, 2))
    {
        return None;
    }
    let separator = b[10];
    if !matches!(separator, b'T' | b't' | b' ') {
        return None;
    }

    let year: i64 = parse_digits(&b[0..4])? as i64;
    let month: u32 = parse_digits(&b[5..7])?;
    let day: u32 = parse_digits(&b[8..10])?;
    let hour: u32 = parse_digits(&b[11..13])?;
    let minute: u32 = parse_digits(&b[14..16])?;
    let second: u32 = parse_digits(&b[17..19])?;
    if !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    let mut pos = 19;
    // Optional fractional seconds (up to 9 digits, dropped in the output).
    if pos < b.len() && b[pos] == b'.' {
        pos += 1;
        let mut frac_digits = 0;
        while pos < b.len() && b[pos].is_ascii_digit() {
            frac_digits += 1;
            if frac_digits > 9 {
                return None;
            }
            pos += 1;
        }
        if frac_digits == 0 {
            return None;
        }
    }

    // Offset: absent only for the SQLite layout (→ UTC); 'Z' or '±HH:MM'
    // for RFC3339 (which requires the offset).
    let offset_minutes: i64 = if pos == b.len() {
        if separator == b' ' {
            0
        } else {
            return None;
        }
    } else if separator == b' ' {
        return None; // no offsets or trailing data in the SQLite layout
    } else if matches!(b[pos], b'Z' | b'z') {
        if pos + 1 != b.len() {
            return None;
        }
        0
    } else if matches!(b[pos], b'+' | b'-') {
        if b.len() != pos + 6 || b[pos + 3] != b':' {
            return None;
        }
        let offset_hours = parse_digits(&b[pos + 1..pos + 3])?;
        let offset_minutes_part = parse_digits(&b[pos + 4..pos + 6])?;
        if offset_hours > 23 || offset_minutes_part > 59 {
            return None;
        }
        let sign = if b[pos] == b'+' { 1 } else { -1 };
        sign * (offset_hours as i64 * 60 + offset_minutes_part as i64)
    } else {
        return None;
    };

    Some(format_rfc3339(
        year,
        month,
        day,
        hour,
        minute,
        second,
        offset_minutes,
    ))
}

/// Parse a run of ASCII digits into a value; `None` on an empty slice or a
/// non-digit byte (no overflow: timestamps are short).
fn parse_digits(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() {
        return None;
    }
    let mut value: u32 = 0;
    for &c in bytes {
        let digit = c.checked_sub(b'0')?;
        if digit > 9 {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u32::from(digit))?;
    }
    Some(value)
}

/// Days in a month (proleptic Gregorian, leap-year aware); 0 for an
/// out-of-range month.
fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Render the canonical RFC3339 form: `Z` for a zero offset, `±HH:MM`
/// otherwise (the oracle's `time.Format(time.RFC3339)` behavior).
fn format_rfc3339(
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    offset_minutes: i64,
) -> String {
    let mut out = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}");
    if offset_minutes == 0 {
        out.push('Z');
    } else {
        let sign = if offset_minutes < 0 { '-' } else { '+' };
        let absolute = offset_minutes.unsigned_abs();
        out.push_str(&format!("{sign}{:02}:{:02}", absolute / 60, absolute % 60));
    }
    out
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

    // updated_at normalization: all three accepted layouts, and the
    // unparseable inputs that must skip the key (oracle
    // TestEnricher_NormalizesUpdatedAt / TestEnricher_InvalidUpdatedAtSkipped).
    #[test]
    fn normalize_updated_at_formats() {
        let cases = [
            // (input, expected)
            ("2026-08-01 12:00:00", Some("2026-08-01T12:00:00Z")),
            ("2026-08-01 12:00:00.123", Some("2026-08-01T12:00:00Z")),
            (
                "2026-08-01 12:00:00.123456789",
                Some("2026-08-01T12:00:00Z"),
            ),
            ("2026-08-01T12:00:00Z", Some("2026-08-01T12:00:00Z")),
            ("2026-08-01T12:00:00z", Some("2026-08-01T12:00:00Z")),
            ("2026-08-01T12:00:00.5Z", Some("2026-08-01T12:00:00Z")),
            (
                "2026-08-01T12:00:00+02:00",
                Some("2026-08-01T12:00:00+02:00"),
            ),
            (
                "2026-08-01T12:00:00-05:30",
                Some("2026-08-01T12:00:00-05:30"),
            ),
            // Unparseable → the key is skipped.
            ("", None),
            ("not-a-date", None),
            ("2026-13-01 12:00:00", None),
            ("2026-02-30 12:00:00", None),
            ("2024-02-29 12:00:00", Some("2024-02-29T12:00:00Z")),
            ("2026-02-29 12:00:00", None),
            ("2026-08-01T25:00:00Z", None),
            ("2026-08-01 12:00:60", None),
            ("2026-08-01 12:00:00Z", None),
            ("2026-08-01T12:00:00", None),
            ("2026-08-01 12:00:00.", None),
            ("2026-08-01 12:00:00.1234567890", None),
        ];
        for (input, expected) in cases {
            assert_eq!(
                normalize_updated_at(input).as_deref(),
                expected,
                "normalize_updated_at({input:?})"
            );
        }
    }

    // Reranker flags: right types extracted (false is a value, not an
    // absence), wrong types and empty valid_to skipped (oracle
    // TestEnricher_MetadataFlags).
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

    // End-to-end (oracle TestEnricher_InvalidUpdatedAtSkipped): an
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
    // every result is enriched from the shared batched lookups (the
    // oracle's FIX-037 single-query property holds by construction: one
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
