//! Lexical (FTS5/BM25) sub-search leg.
//!
//! A thin wrapper over [`db::ChunkDao::search_fts`]: bm25 ranking, the domain
//! filter and the limit clamping all live SQL-side in the DAO (it orders by
//! `bm25()` and applies the domain `EXISTS` before `LIMIT`).
//!
//! **Conscious deviations from the oracle:**
//! - an empty *or whitespace-only* query returns `Ok(empty)` (the oracle
//!   checked only `== ""`; a whitespace-only FTS5 MATCH expression would be a
//!   syntax error at the DAO);
//! - the domain is normalized (`crate::normalize_domain`) before the SQL
//!   comparison: the oracle passed the raw string through, so the SQL
//!   `json_each.value = ?` comparison was case-sensitive even though the
//!   oracle's app-side `filterByDomain` normalized both sides. Stored domains
//!   are canonically lowercase (ontology XML), so normalizing the input makes
//!   the filter case-insensitive without touching the db crate.

use db::{ChunkDao, FtsHit};

use crate::{LexicalHit, SearchError, normalize_domain};

/// Lexical sub-searcher over the FTS5 index.
///
/// One instance per unit of work, borrowing the caller's [`ChunkDao`]
/// (connection- or transaction-bound per the db crate's design D2).
pub struct LexicalSearcher<'conn> {
    chunks: &'conn ChunkDao<'conn>,
}

impl<'conn> LexicalSearcher<'conn> {
    /// Bind the searcher to a chunk DAO.
    pub fn new(chunks: &'conn ChunkDao<'conn>) -> Self {
        Self { chunks }
    }

    /// Ranked FTS5 search.
    ///
    /// `query` is an FTS5 MATCH expression (plain term, `"phrase"`, `a OR b`,
    /// …) passed through to the DAO verbatim. An empty or whitespace-only
    /// query returns an empty vec. `top_k` is clamped by the DAO's page
    /// contract (`<= 0` or `> 100` → 20). `domain` restricts the result to
    /// documents whose `metadata_json` `$.domain` matches it (case-insensitive);
    /// `None` disables the filter.
    pub fn search(
        &self,
        query: &str,
        top_k: i64,
        domain: Option<&str>,
    ) -> Result<Vec<LexicalHit>, SearchError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let domain = normalize_domain(domain);
        let hits = self.chunks.search_fts(query, top_k, domain.as_deref())?;
        Ok(hits.into_iter().map(LexicalHit::from).collect())
    }
}

/// Map a DAO hit to a sub-search hit (chunk fields + raw bm25 score, where
/// lower is better). The `chunk_text` field carries the chunk's pure text
/// (the byte-offset slice) — the FTS5 index matched on `search_text`, which
/// is not carried (chunk-metadata-persistence design D5). The raw
/// `metadata_json` bag is carried for the result's `chunk_metadata`.
impl From<FtsHit> for LexicalHit {
    fn from(hit: FtsHit) -> Self {
        let db::Chunk {
            id,
            doc_id,
            chunk_text,
            metadata_json,
            sequence_num,
            start_offset,
            end_offset,
            ..
        } = hit.chunk;
        Self {
            chunk_id: id,
            chunk_text,
            metadata_json,
            document_id: doc_id,
            sequence_num,
            start_offset,
            end_offset,
            score: hit.score,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use db::test_util::in_memory_db;
    use db::{ConnectionOrTx, Db, DocumentDao};

    use super::*;

    /// Create a document with `metadata_json` and return its id.
    fn seed_doc(db: &Db, path: &str, metadata_json: Option<&str>) -> i64 {
        db.exec_tx(|tx| {
            let docs = DocumentDao::new(ConnectionOrTx::Transaction(&*tx));
            docs.create("markdown", path, metadata_json, None)
        })
        .expect("seed document commits")
    }

    /// Insert one chunk (outside the searcher's connection) and return its id.
    fn seed_chunk(db: &Db, doc_id: i64, text: &str, seq: i64) -> i64 {
        db.exec_tx(|tx| {
            let chunks = ChunkDao::new(ConnectionOrTx::Transaction(&*tx));
            chunks.create(doc_id, text, seq, None, None)
        })
        .expect("seed chunk commits")
    }

    /// Run `f` with a searcher bound to a pooled connection.
    fn with_lexical<T>(db: &Db, f: impl FnOnce(&LexicalSearcher<'_>) -> T) -> T {
        db.with_conn(|conn| {
            let chunks = ChunkDao::new(ConnectionOrTx::Connection(conn));
            f(&LexicalSearcher::new(&chunks))
        })
        .unwrap()
    }

    // Happy path: bm25 ranking, field mapping (including offsets), top_k.
    #[test]
    fn search_returns_ranked_hits_with_mapped_fields() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/docs/a.md", None);
        seed_chunk(&db, doc, "alpha beta one", 0);
        seed_chunk(&db, doc, "alpha beta two", 1);
        seed_chunk(&db, doc, "alpha gamma three", 2);

        with_lexical(&db, |lexical| {
            let hits = lexical.search("alpha", 20, None).unwrap();
            assert_eq!(hits.len(), 3);
            // bm25 rank: scores non-decreasing (more negative = better).
            for (cur, next) in hits.iter().zip(hits.iter().skip(1)) {
                assert!(cur.score <= next.score, "results must be bm25-ranked");
            }

            // Field mapping, including offsets (set on the first chunk).
            let mapped = hits
                .iter()
                .find(|h| h.chunk_text == "alpha beta one")
                .unwrap();
            assert!(mapped.chunk_id > 0);
            assert_eq!(mapped.document_id, doc);
            assert_eq!(mapped.sequence_num, 0);
            let unmapped = hits
                .iter()
                .find(|h| h.chunk_text == "alpha beta two")
                .unwrap();
            assert_eq!(unmapped.start_offset, None);
            assert_eq!(unmapped.end_offset, None);

            // top_k is honored (in-range values pass through to the DAO).
            let top2 = lexical.search("alpha", 2, None).unwrap();
            assert_eq!(top2.len(), 2);

            // No match → empty vec, not an error.
            assert!(lexical.search("nomatch", 20, None).unwrap().is_empty());
        });
    }

    // Offsets are carried through from the chunk row.
    #[test]
    fn search_maps_start_and_end_offsets() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/docs/a.md", None);
        db.exec_tx(|tx| {
            let chunks = ChunkDao::new(ConnectionOrTx::Transaction(&*tx));
            chunks.create(doc, "offset chunk", 0, Some(7), Some(19))
        })
        .expect("seed chunk commits");

        with_lexical(&db, |lexical| {
            let hits = lexical.search("offset", 20, None).unwrap();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].start_offset, Some(7));
            assert_eq!(hits[0].end_offset, Some(19));
        });
    }

    // Domain filter: in-domain only, no-match empty, None → all.
    #[test]
    fn search_domain_filter() {
        let db = in_memory_db();
        let hr = seed_doc(&db, "/docs/hr.md", Some(r#"{"domain":"hr"}"#));
        let eng = seed_doc(&db, "/docs/eng.md", Some(r#"{"domain":"engineering"}"#));
        seed_chunk(&db, hr, "hr policy chunk", 0);
        seed_chunk(&db, hr, "hr benefits chunk", 1);
        seed_chunk(&db, eng, "engineering spec chunk", 0);
        seed_chunk(&db, eng, "engineering design chunk", 1);

        with_lexical(&db, |lexical| {
            assert_eq!(lexical.search("chunk", 20, Some("hr")).unwrap().len(), 2);
            assert_eq!(
                lexical
                    .search("chunk", 20, Some("engineering"))
                    .unwrap()
                    .len(),
                2
            );
            assert_eq!(
                lexical.search("chunk", 20, Some("product")).unwrap().len(),
                0
            );
            assert_eq!(lexical.search("chunk", 20, None).unwrap().len(), 4);
        });
    }

    // Deviation: the domain comparison is case-insensitive (the oracle's SQL
    // pass-through was case-sensitive).
    #[test]
    fn search_domain_filter_is_case_insensitive() {
        let db = in_memory_db();
        let hr = seed_doc(&db, "/docs/hr.md", Some(r#"{"domain":"hr"}"#));
        seed_chunk(&db, hr, "hr policy chunk", 0);
        seed_chunk(&db, hr, "hr benefits chunk", 1);

        with_lexical(&db, |lexical| {
            assert_eq!(lexical.search("chunk", 20, Some("HR ")).unwrap().len(), 2);
            assert_eq!(
                lexical.search("chunk", 20, Some("  hr\t")).unwrap().len(),
                2
            );
        });
    }

    // Deviation: whitespace-only queries are empty queries (the oracle only
    // checked `== ""`).
    #[test]
    fn search_empty_query_returns_empty() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/docs/a.md", None);
        seed_chunk(&db, doc, "zebra stripes", 0);

        with_lexical(&db, |lexical| {
            assert!(lexical.search("", 20, None).unwrap().is_empty());
            assert!(lexical.search("   \t ", 20, None).unwrap().is_empty());
        });
    }

    // A malformed FTS5 MATCH expression surfaces as the DAO error.
    // (A chunk must exist: on an EMPTY FTS index SQLite short-circuits the
    // MATCH and never parses the query expression.)
    #[test]
    fn search_propagates_dao_errors() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/docs/a.md", None);
        seed_chunk(&db, doc, "zebra stripes", 0);

        with_lexical(&db, |lexical| {
            let err = lexical.search("unclosed \"phrase", 20, None).unwrap_err();
            assert!(matches!(err, SearchError::Db(_)), "got: {err:?}");
        });
    }

    // FtsHit → LexicalHit maps every field (created_at is not a hit field):
    // the chunk_text field carries the pure body (not search_text) and the
    // raw metadata_json is carried (chunk-metadata-persistence design D5).
    #[test]
    fn from_fts_hit_maps_all_fields() {
        let chunk = db::Chunk {
            id: 42,
            doc_id: 7,
            chunk_text: "raw body".to_string(),
            search_text: "Breadcrumb\n\nraw body".to_string(),
            metadata_json: Some(r#"{"breadcrumb":"Breadcrumb"}"#.to_owned()),
            sequence_num: 3,
            start_offset: Some(1),
            end_offset: Some(4),
            created_at: "2026-01-01 00:00:00".to_string(),
        };
        let hit = LexicalHit::from(FtsHit { chunk, score: -1.5 });
        assert_eq!(hit.chunk_id, 42);
        assert_eq!(
            hit.chunk_text, "raw body",
            "the hit carries the pure chunk_text"
        );
        assert_eq!(
            hit.metadata_json.as_deref(),
            Some(r#"{"breadcrumb":"Breadcrumb"}"#),
            "the raw metadata_json is carried"
        );
        assert_eq!(hit.document_id, 7);
        assert_eq!(hit.sequence_num, 3);
        assert_eq!(hit.start_offset, Some(1));
        assert_eq!(hit.end_offset, Some(4));
        assert_eq!(hit.score, -1.5);
    }

    // The hit carries the chunk's pure chunk_text (the byte-offset slice),
    // not search_text: a seeded chunk with distinct texts matches on the
    // breadcrumb term (the FTS index is over search_text) and returns the
    // pure body (chunk-metadata-persistence design D5).
    #[test]
    fn search_hit_carries_chunk_text() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/docs/a.md", None);
        db.exec_tx(|tx| {
            let chunks = ChunkDao::new(ConnectionOrTx::Transaction(&*tx));
            chunks.create_with_search_text(
                doc,
                "zebra stripes",
                "Atlas Guide\n\nzebra stripes",
                None,
                0,
                None,
                None,
            )
        })
        .expect("seed chunk commits");

        with_lexical(&db, |lexical| {
            let hits = lexical.search("atlas", 20, None).unwrap();
            assert_eq!(hits.len(), 1, "the breadcrumb term matches via search_text");
            assert_eq!(
                hits[0].chunk_text, "zebra stripes",
                "the hit carries the pure chunk_text, not search_text"
            );
        });
    }
}
