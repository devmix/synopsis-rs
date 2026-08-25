//! Semantic (vector) sub-search leg.
//!
//! Oracle mapping: `../synopsis/internal/search/semantic_search.go`,
//! re-architected for the lancedb-backed index (design D3): the oracle's vec0
//! `SearchVector` filtered by domain inside SQL; our [`vectors::VectorIndex`]
//! is domain-blind, so the domain filter moves to the application side with
//! over-fetch.
//!
//! Flow: embed the query (an empty embedding is an error) → top-k index
//! search with `topK × [`OVERFETCH_FACTOR`]` → resolve chunk rows, skipping
//! orphaned vectors (the vectors crate's cascade protocol) → when a domain is
//! requested, resolve document domains from `metadata_json` and filter by
//! normalized domain → truncate to `topK`.
//!
//! **Conscious deviations from the oracle:**
//! - application-side domain filtering with over-fetch (design D3);
//! - chunk resolution is per-id [`db::ChunkDao::get_by_id`]: the DAO has no
//!   batch `get_by_ids`, and the round-trips are cheap on local SQLite with a
//!   count bounded by `topK × 3` (a batch method is a db-crate addition, out
//!   of scope here);
//! - a non-positive `top_k` falls back to [`DEFAULT_TOP_K`] (the config
//!   validation default) instead of reaching the index, which rejects
//!   `k == 0`;
//! - an empty or whitespace-only query returns `Ok(empty)` (same as the
//!   lexical leg); the oracle checked only `== ""`.
//!
//! **Over-fetch starvation:** [`OVERFETCH_FACTOR`] = 3 bounds the candidate
//! pool at `topK × 3`, so a domain-starved corpus (fewer than ~1/3 of the
//! nearest neighbors in the requested domain) may yield fewer than `topK`
//! results even when more in-domain chunks exist globally.
//!
//! Domains come from the shared [`crate::document_domains`] helper (moved
//! here → crate root in task 4.3, shared with the enricher).

use std::collections::HashMap;

use db::{ChunkDao, DocumentDao};
use embedding::EmbeddingProvider;
use vectors::VectorIndex;

use crate::{SearchError, SemanticHit, document_domains, normalize_domain};

/// Over-fetch factor for the domain-blind index (design D3): the leg fetches
/// `topK × 3` candidates so that enough in-domain hits survive the
/// application-side filter (and orphaned vectors cannot eat into `topK`).
const OVERFETCH_FACTOR: usize = 3;

/// Fallback for a non-positive `top_k`: the default `semantic_top_k` applied
/// by config validation (the index itself rejects `k == 0`).
const DEFAULT_TOP_K: usize = 20;

/// Semantic sub-searcher over an embedding provider and an ANN index.
///
/// One instance per unit of work, borrowing the caller's DAOs
/// (connection- or transaction-bound per the db crate's design D2) and the
/// shared provider/index handles.
pub struct SemanticSearcher<'conn> {
    chunks: &'conn ChunkDao<'conn>,
    documents: &'conn DocumentDao<'conn>,
    provider: &'conn dyn EmbeddingProvider,
    index: &'conn dyn VectorIndex,
}

impl<'conn> SemanticSearcher<'conn> {
    /// Bind the searcher to its collaborators.
    pub fn new(
        chunks: &'conn ChunkDao<'conn>,
        documents: &'conn DocumentDao<'conn>,
        provider: &'conn dyn EmbeddingProvider,
        index: &'conn dyn VectorIndex,
    ) -> Self {
        Self {
            chunks,
            documents,
            provider,
            index,
        }
    }

    /// Vector similarity search.
    ///
    /// An empty or whitespace-only query returns an empty vec without
    /// embedding or querying the index. `top_k` is the number of hits to
    /// return (non-positive values fall back to [`DEFAULT_TOP_K`]); the index
    /// is asked for `topK × [`OVERFETCH_FACTOR`]`. `domain` restricts the
    /// result to documents whose `metadata_json` `$.domain` matches it
    /// (case-insensitive); `None` disables the filter.
    pub fn search(
        &self,
        query: &str,
        top_k: i64,
        domain: Option<&str>,
    ) -> Result<Vec<SemanticHit>, SearchError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let domain = normalize_domain(domain);
        let top_k = if top_k > 0 {
            top_k as usize
        } else {
            DEFAULT_TOP_K
        };
        let fetch_k = top_k * OVERFETCH_FACTOR;

        let embeddings = self.provider.generate_embeddings(&[query.to_string()])?;
        let embedding = embeddings
            .first()
            .filter(|vector| !vector.is_empty())
            .ok_or_else(|| {
                SearchError::Semantic("empty embedding generated for query".to_string())
            })?;

        let hits = self.index.search(embedding, fetch_k)?;
        if hits.is_empty() {
            return Ok(Vec::new());
        }

        // Resolve chunk rows; skip orphaned vectors (cascade protocol).
        let mut resolved = Vec::with_capacity(hits.len());
        for &(chunk_id, distance) in &hits {
            if let Some(chunk) = self.chunks.get_by_id(chunk_id as i64)? {
                resolved.push((chunk, distance));
            }
        }
        if resolved.is_empty() {
            return Ok(Vec::new());
        }

        // Application-side domain filter (design D3), before truncation.
        if let Some(domain) = &domain {
            let doc_ids: Vec<i64> = resolved.iter().map(|(chunk, _)| chunk.doc_id).collect();
            let docs = self.documents.get_by_ids(&doc_ids)?;
            let domains_by_doc: HashMap<i64, Vec<String>> = docs
                .into_iter()
                .map(|doc| (doc.id, document_domains(doc.metadata_json.as_deref())))
                .collect();
            resolved.retain(|(chunk, _)| {
                domains_by_doc
                    .get(&chunk.doc_id)
                    .is_some_and(|domains| domains.iter().any(|d| d == domain))
            });
        }

        resolved.truncate(top_k);

        Ok(resolved
            .into_iter()
            .map(|(chunk, distance)| SemanticHit {
                chunk_id: chunk.id,
                chunk_text: chunk.chunk_text,
                document_id: chunk.doc_id,
                sequence_num: chunk.sequence_num,
                start_offset: chunk.start_offset,
                end_offset: chunk.end_offset,
                score: distance as f64,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::atomic::{AtomicUsize, Ordering};

    use db::test_util::in_memory_db;
    use db::{ConnectionOrTx, Db, DocumentDao};
    use embedding::EmbeddingError;
    use vectors::VectorsError;

    use super::*;

    /// Canned embedding provider: one fixed vector per call, or a failure.
    struct MockProvider {
        embedding: Vec<f32>,
        fail: bool,
        calls: AtomicUsize,
    }

    impl EmbeddingProvider for MockProvider {
        fn generate_embeddings(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            assert_eq!(texts.len(), 1, "the leg embeds exactly one query text");
            if self.fail {
                return Err(EmbeddingError::Model("mock provider failure".to_string()));
            }
            Ok(vec![self.embedding.clone()])
        }

        fn vector_dim(&self) -> usize {
            self.embedding.len()
        }

        fn name(&self) -> &'static str {
            "mock"
        }
    }

    /// Failure modes the canned index can report.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum IndexFailure {
        DimensionMismatch { expected: usize, actual: usize },
        Engine,
    }

    /// Canned index: fixed hits in distance order, or a canned failure;
    /// records the `k` of the last search.
    struct MockIndex {
        hits: Vec<(u32, f32)>,
        fail_with: Option<IndexFailure>,
        last_k: AtomicUsize,
    }

    impl VectorIndex for MockIndex {
        fn search(&self, _query: &[f32], k: usize) -> Result<Vec<(u32, f32)>, VectorsError> {
            self.last_k.store(k, Ordering::Relaxed);
            match self.fail_with {
                Some(IndexFailure::DimensionMismatch { expected, actual }) => {
                    Err(VectorsError::DimensionMismatch { expected, actual })
                }
                Some(IndexFailure::Engine) => {
                    Err(VectorsError::Engine("mock index failure".to_string()))
                }
                None => Ok(self.hits.clone()),
            }
        }

        fn insert(&self, _chunk_id: u32, _vector: &[f32]) -> Result<(), VectorsError> {
            Err(VectorsError::Engine(
                "mock index is search-only".to_string(),
            ))
        }

        fn insert_batch(&self, _rows: &[(u32, &[f32])]) -> Result<(), VectorsError> {
            Err(VectorsError::Engine(
                "mock index is search-only".to_string(),
            ))
        }

        fn delete_by_chunk_ids(&self, _chunk_ids: &[u32]) -> Result<(), VectorsError> {
            Err(VectorsError::Engine(
                "mock index is search-only".to_string(),
            ))
        }

        fn chunk_ids(&self) -> Result<Vec<u32>, VectorsError> {
            Err(VectorsError::Engine(
                "mock index is search-only".to_string(),
            ))
        }

        fn count(&self) -> Result<u64, VectorsError> {
            Err(VectorsError::Engine(
                "mock index is search-only".to_string(),
            ))
        }

        fn build_index(&self) -> Result<(), VectorsError> {
            Err(VectorsError::Engine(
                "mock index is search-only".to_string(),
            ))
        }

        fn rebuild(&self, _rows: &[(u32, Vec<f32>)]) -> Result<(), VectorsError> {
            Err(VectorsError::Engine(
                "mock index is search-only".to_string(),
            ))
        }
    }

    /// Create a document with `metadata_json` and return its id.
    fn seed_doc(db: &Db, path: &str, metadata_json: Option<&str>) -> i64 {
        db.exec_tx(|tx| {
            let docs = DocumentDao::new(ConnectionOrTx::Transaction(&*tx));
            docs.create("markdown", path, metadata_json, None)
        })
        .expect("seed document commits")
    }

    /// Insert one chunk and return its id.
    fn seed_chunk(db: &Db, doc_id: i64, text: &str, seq: i64) -> i64 {
        db.exec_tx(|tx| {
            let chunks = ChunkDao::new(ConnectionOrTx::Transaction(&*tx));
            chunks.create(doc_id, text, seq, None, None)
        })
        .expect("seed chunk commits")
    }

    fn provider_with(embedding: Vec<f32>) -> MockProvider {
        MockProvider {
            embedding,
            fail: false,
            calls: AtomicUsize::new(0),
        }
    }

    fn index_with(hits: Vec<(u32, f32)>) -> MockIndex {
        MockIndex {
            hits,
            fail_with: None,
            last_k: AtomicUsize::new(0),
        }
    }

    /// Run `f` with a searcher bound to pooled connections and the given
    /// provider/index.
    fn with_semantic<T>(
        db: &Db,
        provider: &MockProvider,
        index: &MockIndex,
        f: impl FnOnce(&SemanticSearcher<'_>) -> T,
    ) -> T {
        db.with_conn(|conn| {
            let chunks = ChunkDao::new(ConnectionOrTx::Connection(conn));
            let documents = DocumentDao::new(ConnectionOrTx::Connection(conn));
            f(&SemanticSearcher::new(&chunks, &documents, provider, index))
        })
        .unwrap()
    }

    /// The six-chunk domain fixture: three chunks each in the `hr` and the
    /// `engineering` document.
    fn seed_domain_fixture(db: &Db) -> (Vec<i64>, Vec<i64>) {
        let hr_doc = seed_doc(db, "/docs/hr.md", Some(r#"{"domain":"hr"}"#));
        let eng_doc = seed_doc(db, "/docs/eng.md", Some(r#"{"domain":"engineering"}"#));
        let hr: Vec<i64> = (0..3)
            .map(|i| seed_chunk(db, hr_doc, &format!("hr chunk {i}"), i as i64))
            .collect();
        let eng: Vec<i64> = (0..3)
            .map(|i| seed_chunk(db, eng_doc, &format!("engineering chunk {i}"), i as i64))
            .collect();
        (hr, eng)
    }

    // Happy path: index order preserved, fields mapped, over-fetch requested.
    #[test]
    fn search_returns_index_ordered_hits_with_mapped_fields() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/docs/a.md", None);
        let c1 = seed_chunk(&db, doc, "first chunk text", 0);
        let c2 = seed_chunk(&db, doc, "second chunk text", 1);
        let provider = provider_with(vec![1.0, 0.0, 0.0]);
        let index = index_with(vec![(c2 as u32, 0.25), (c1 as u32, 0.75)]);

        with_semantic(&db, &provider, &index, |semantic| {
            let hits = semantic.search("query", 20, None).unwrap();
            assert_eq!(hits.len(), 2);
            assert_eq!(hits[0].chunk_id, c2);
            assert_eq!(hits[0].chunk_text, "second chunk text");
            assert_eq!(hits[0].document_id, doc);
            assert_eq!(hits[0].sequence_num, 1);
            assert_eq!(hits[0].score, 0.25);
            assert_eq!(hits[1].chunk_id, c1);
            assert_eq!(hits[1].score, 0.75);
            assert_eq!(provider.calls.load(Ordering::Relaxed), 1);
            assert_eq!(
                index.last_k.load(Ordering::Relaxed),
                20 * OVERFETCH_FACTOR,
                "the index must be asked for topK × OVERFETCH_FACTOR"
            );
        });
    }

    // Empty and whitespace-only queries: no embedding, no index call.
    #[test]
    fn search_empty_query_returns_empty_without_calls() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/docs/a.md", None);
        let c1 = seed_chunk(&db, doc, "zebra", 0);
        let provider = provider_with(vec![1.0]);
        let index = index_with(vec![(c1 as u32, 0.1)]);

        with_semantic(&db, &provider, &index, |semantic| {
            assert!(semantic.search("", 20, None).unwrap().is_empty());
            assert!(semantic.search("   \t ", 20, None).unwrap().is_empty());
            assert_eq!(provider.calls.load(Ordering::Relaxed), 0);
            assert_eq!(index.last_k.load(Ordering::Relaxed), 0);
        });
    }

    // An empty embedding is an error and never reaches the index.
    #[test]
    fn search_empty_embedding_is_error() {
        let db = in_memory_db();
        let provider = provider_with(Vec::new());
        let index = index_with(Vec::new());

        with_semantic(&db, &provider, &index, |semantic| {
            let err = semantic.search("query", 20, None).unwrap_err();
            assert!(
                matches!(
                    err,
                    SearchError::Semantic(ref msg) if msg.contains("empty embedding")
                ),
                "got: {err:?}"
            );
            assert_eq!(index.last_k.load(Ordering::Relaxed), 0);
        });
    }

    // Provider and index failures surface with their crate error types.
    #[test]
    fn search_error_propagation() {
        let db = in_memory_db();

        let failing_provider = MockProvider {
            embedding: vec![1.0],
            fail: true,
            calls: AtomicUsize::new(0),
        };
        let index = index_with(Vec::new());
        with_semantic(&db, &failing_provider, &index, |semantic| {
            let err = semantic.search("query", 20, None).unwrap_err();
            assert!(matches!(err, SearchError::Embedding(_)), "got: {err:?}");
        });

        let provider = provider_with(vec![1.0]);
        let failing_index = MockIndex {
            hits: Vec::new(),
            fail_with: Some(IndexFailure::Engine),
            last_k: AtomicUsize::new(0),
        };
        with_semantic(&db, &provider, &failing_index, |semantic| {
            let err = semantic.search("query", 20, None).unwrap_err();
            assert!(
                matches!(err, SearchError::Vector(VectorsError::Engine(_))),
                "got: {err:?}"
            );
        });

        let mismatch_index = MockIndex {
            hits: Vec::new(),
            fail_with: Some(IndexFailure::DimensionMismatch {
                expected: 1024,
                actual: 3,
            }),
            last_k: AtomicUsize::new(0),
        };
        with_semantic(&db, &provider, &mismatch_index, |semantic| {
            match semantic.search("query", 20, None).unwrap_err() {
                SearchError::Vector(VectorsError::DimensionMismatch { expected, actual }) => {
                    assert_eq!((expected, actual), (1024, 3))
                }
                other => panic!("expected DimensionMismatch, got: {other:?}"),
            }
        });
    }

    // D3 core: the index returns out-of-domain chunks first — a naive topK
    // fetch would return nothing in-domain; the ×3 over-fetch carries the
    // in-domain hits, which are then truncated to topK in index order.
    #[test]
    fn search_domain_filter_overfetches_then_truncates() {
        let db = in_memory_db();
        let (hr, eng) = seed_domain_fixture(&db);
        let provider = provider_with(vec![1.0]);
        let index = index_with(vec![
            (eng[0] as u32, 0.1),
            (eng[1] as u32, 0.2),
            (eng[2] as u32, 0.3),
            (hr[0] as u32, 0.4),
            (hr[1] as u32, 0.5),
            (hr[2] as u32, 0.6),
        ]);

        with_semantic(&db, &provider, &index, |semantic| {
            let hits = semantic.search("query", 2, Some("hr")).unwrap();
            assert_eq!(
                index.last_k.load(Ordering::Relaxed),
                2 * OVERFETCH_FACTOR,
                "the index must be asked for topK × OVERFETCH_FACTOR"
            );
            let ids: Vec<i64> = hits.iter().map(|h| h.chunk_id).collect();
            assert_eq!(
                ids,
                vec![hr[0], hr[1]],
                "in-domain hits in index order, truncated to topK"
            );
        });
    }

    // Domain filtering is case-insensitive (both sides normalized).
    #[test]
    fn search_domain_filter_is_case_insensitive() {
        let db = in_memory_db();
        let (hr, _eng) = seed_domain_fixture(&db);
        let provider = provider_with(vec![1.0]);
        let index = index_with(
            hr.iter()
                .enumerate()
                .map(|(i, id)| (*id as u32, i as f32))
                .collect(),
        );

        with_semantic(&db, &provider, &index, |semantic| {
            let hits = semantic.search("query", 20, Some("HR ")).unwrap();
            let ids: Vec<i64> = hits.iter().map(|h| h.chunk_id).collect();
            assert_eq!(ids, hr);
        });
    }

    // No domain: no document lookup, everything resolved, truncated to topK.
    #[test]
    fn search_without_domain_returns_all_resolved() {
        let db = in_memory_db();
        let (hr, eng) = seed_domain_fixture(&db);
        let provider = provider_with(vec![1.0]);
        let mut hits = Vec::new();
        hits.extend(eng.iter().enumerate().map(|(i, id)| (*id as u32, i as f32)));
        hits.extend(
            hr.iter()
                .enumerate()
                .map(|(i, id)| (*id as u32, 3.0 + i as f32)),
        );
        let index = index_with(hits);

        with_semantic(&db, &provider, &index, |semantic| {
            let hits = semantic.search("query", 4, None).unwrap();
            assert_eq!(hits.len(), 4, "truncated to topK");
            let ids: Vec<i64> = hits.iter().map(|h| h.chunk_id).collect();
            let mut expected = eng.clone();
            expected.push(hr[0]);
            assert_eq!(ids, expected, "index order preserved, truncated to topK");
        });
    }

    // Cascade protocol: index hits whose chunk row is gone are skipped.
    #[test]
    fn search_skips_orphaned_vectors() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/docs/a.md", None);
        let c1 = seed_chunk(&db, doc, "real chunk", 0);
        let c2 = seed_chunk(&db, doc, "another real chunk", 1);
        let provider = provider_with(vec![1.0]);
        let index = index_with(vec![(c1 as u32, 0.1), (999_999, 0.2), (c2 as u32, 0.3)]);

        with_semantic(&db, &provider, &index, |semantic| {
            let hits = semantic.search("query", 20, None).unwrap();
            let ids: Vec<i64> = hits.iter().map(|h| h.chunk_id).collect();
            assert_eq!(ids, vec![c1, c2], "orphaned vector skipped");
        });
    }

    /// Insert a chunk whose `doc_id` references no `documents` row: the FK is
    /// temporarily disabled on the pooled connection (the state is otherwise
    /// unreachable under the enforced FK).
    fn insert_orphan_document_chunk(db: &Db, doc_id: i64, text: &str) -> i64 {
        db.with_conn(|conn| -> Result<i64, db::DbError> {
            conn.execute_batch("PRAGMA foreign_keys = OFF")?;
            let chunks = ChunkDao::new(ConnectionOrTx::Connection(conn));
            let id = chunks.create(doc_id, text, 0, None, None)?;
            conn.execute_batch("PRAGMA foreign_keys = ON")?;
            Ok(id)
        })
        .expect("connection checkout")
        .expect("orphan-document chunk inserted")
    }

    // A domain filter also excludes chunks whose document row is missing
    // (nothing to compare the domain against).
    #[test]
    fn search_domain_filter_excludes_chunks_without_document() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/docs/hr.md", Some(r#"{"domain":"hr"}"#));
        let real = seed_chunk(&db, doc, "real chunk", 0);
        // doc_id 424242 has no row in `documents`.
        let orphan_doc = insert_orphan_document_chunk(&db, 424242, "orphan document chunk");
        let provider = provider_with(vec![1.0]);
        let index = index_with(vec![(orphan_doc as u32, 0.1), (real as u32, 0.2)]);

        with_semantic(&db, &provider, &index, |semantic| {
            let hits = semantic.search("query", 20, Some("hr")).unwrap();
            let ids: Vec<i64> = hits.iter().map(|h| h.chunk_id).collect();
            assert_eq!(ids, vec![real]);
        });
    }

    // Non-positive top_k falls back to the default (the index rejects k == 0).
    #[test]
    fn search_non_positive_top_k_uses_default() {
        let db = in_memory_db();
        let provider = provider_with(vec![1.0]);
        let index = index_with(Vec::new());

        with_semantic(&db, &provider, &index, |semantic| {
            assert!(semantic.search("query", 0, None).unwrap().is_empty());
            assert_eq!(
                index.last_k.load(Ordering::Relaxed),
                DEFAULT_TOP_K * OVERFETCH_FACTOR
            );
            assert!(semantic.search("query", -5, None).unwrap().is_empty());
            assert_eq!(
                index.last_k.load(Ordering::Relaxed),
                DEFAULT_TOP_K * OVERFETCH_FACTOR
            );
        });
    }

    // document_domains: both JSON shapes, normalization, malformed input.
    #[test]
    fn document_domains_shapes() {
        assert!(document_domains(None).is_empty(), "no metadata → none");
        assert!(
            document_domains(Some("{not valid json")).is_empty(),
            "malformed JSON → none"
        );
        assert!(
            document_domains(Some(r#"{"other":1}"#)).is_empty(),
            "missing $.domain → none"
        );
        assert_eq!(
            document_domains(Some(r#"{"domain":"HR"}"#)),
            vec!["hr"],
            "scalar string, normalized"
        );
        assert!(
            document_domains(Some(r#"{"domain":""}"#)).is_empty(),
            "empty string → none"
        );
        assert_eq!(
            document_domains(Some(r#"{"domain":["hr","engineering"]}"#)),
            vec!["hr", "engineering"],
            "array of strings"
        );
        assert_eq!(
            document_domains(Some(r#"{"domain":["HR ", 42, " eng ", ""]}"#)),
            vec!["hr", "eng"],
            "non-string and empty members dropped, rest normalized"
        );
    }
}
