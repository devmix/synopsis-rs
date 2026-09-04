//! Hybrid search orchestration: sequential sub-searches, RRF fusion and the
//! finalize pipeline (design D2/D5/D9).
//!
//! [`HybridSearcher`] composes the already-built collaborators — the two
//! sub-search legs ([`LexicalSearcher`] / [`SemanticSearcher`]), the
//! [`Enricher`], the [`Reranker`] and the optional [`GraphExpander`] — and
//! owns the [`SearchConfig`]. Constructor choice (KISS): a single
//! [`HybridSearcher::new`] taking every collaborator; the leg/enricher/
//! expander handles are cheap references, so no builder and no public
//! fields are needed. The CLI assembles the parts once and the searcher is
//! immutable for its lifetime (design D8: no graph-swap API — the
//! CLI rebuilds the searcher after re-indexing).
//!
//! **Hybrid flow** (design D2/D5/D9):
//! 1. empty or whitespace-only query → `Ok(empty)`,
//!    without touching a leg;
//! 2. `top_k <= 0` → `config.final_top_k`;
//! 3. the legs run **sequentially** (no task spawning or timeout plumbing;
//!    `timeout_ms` stays in the frozen config) — the lexical leg when
//!    `enable_lexical`,
//!    the semantic leg when `enable_semantic`; a disabled leg is an empty
//!    success;
//! 4. both legs fail → [`SearchError::BothSubSearchesFailed`] carrying both
//!    causes; one leg fails → a stderr warning and degradation to the
//!    surviving leg;
//! 5. fusion: [`reciprocal_rank_fusion`] with pool
//!    `max(lexical_top_k, semantic_top_k)` (each leg already truncates to
//!    its own top-K, so the pool bound is a defensive cap);
//! 6. finalize (design D5, shared by all three entry points): enrich →
//!    rerank → truncate to topK → graph expand (non-fatal). Domain
//!    filtering happens inside the sub-searches, never after fusion.
//!
//! **Standalone legs**: one leg
//! only, `top_k <= 0` → that leg's config top-K, raw hits mapped to
//! [`SearchResult`] with the score inverted (lower-is-better →
//! higher-is-better, `invert_score`) and 1-based ranks in leg order, then
//! the same finalize pipeline.
//!
//! **Design:**
//! - sequential legs, no timeout (design D2; `timeout_ms` not plumbed);
//! - one-leg degradation logs a warning on stderr (crate convention);
//! - `domain` is `Option<&str>` (the sub-search legs' convention) instead
//!   of an empty-string sentinel;
//! - a non-positive finalize `topK` skips the truncation, so a negative
//!   `FinalTopK` from config is harmless.

use config::preset::SearchConfig;
use serde_json::Map;

use crate::{
    Enricher, GraphExpander, LexicalHit, LexicalSearcher, Reranker, SearchError, SearchResult,
    Searcher, SemanticHit, SemanticSearcher, SourceType, chunk_metadata_bag,
    reciprocal_rank_fusion,
};

/// Orchestrates the hybrid search pipeline (design D2/D5/D9).
///
/// Immutable once built: every collaborator is injected at construction
/// (see the module docs for the choice).
pub struct HybridSearcher<'conn> {
    config: SearchConfig,
    lexical: LexicalSearcher<'conn>,
    semantic: SemanticSearcher<'conn>,
    enricher: Enricher<'conn>,
    reranker: Reranker,
    expander: Option<GraphExpander<'conn>>,
}

impl<'conn> HybridSearcher<'conn> {
    /// Assemble the searcher from its collaborators.
    ///
    /// `expander` is `Some` only when the graph config enables expansion
    /// and a graph index is available (design D8); `None` skips the
    /// expansion stage.
    pub fn new(
        config: SearchConfig,
        lexical: LexicalSearcher<'conn>,
        semantic: SemanticSearcher<'conn>,
        enricher: Enricher<'conn>,
        reranker: Reranker,
        expander: Option<GraphExpander<'conn>>,
    ) -> Self {
        Self {
            config,
            lexical,
            semantic,
            enricher,
            reranker,
            expander,
        }
    }

    /// Hybrid search: both legs (per config flags), RRF fusion, finalize
    /// (design D5). `top_k <= 0` → `config.final_top_k`; an empty or
    /// whitespace-only query returns `Ok(empty)` without touching a leg.
    pub fn hybrid_search(
        &self,
        query: &str,
        top_k: i32,
        domain: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let top_k = if top_k > 0 {
            top_k
        } else {
            self.config.final_top_k
        };

        // Sequential legs (design D2): a disabled leg is an empty success,
        // so only two enabled-and-failed legs produce the hard error.
        let lexical_hits = if self.config.enable_lexical {
            self.lexical
                .search(query, self.config.lexical_top_k as i64, domain)
        } else {
            Ok(Vec::new())
        };
        let semantic_hits = if self.config.enable_semantic {
            self.semantic
                .search(query, self.config.semantic_top_k as i64, domain)
        } else {
            Ok(Vec::new())
        };

        match (lexical_hits, semantic_hits) {
            (Err(lexical_err), Err(semantic_err)) => Err(SearchError::BothSubSearchesFailed {
                lexical: lexical_err.to_string(),
                semantic: semantic_err.to_string(),
            }),
            (Ok(lexical), Ok(semantic)) => self.fuse_and_finalize(&lexical, &semantic, top_k),
            (Ok(lexical), Err(err)) => {
                eprintln!("semantic sub-search failed: {err}; degrading to lexical results only");
                self.fuse_and_finalize(&lexical, &[], top_k)
            }
            (Err(err), Ok(semantic)) => {
                eprintln!("lexical sub-search failed: {err}; degrading to semantic results only");
                self.fuse_and_finalize(&[], &semantic, top_k)
            }
        }
    }

    /// Lexical (FTS5/BM25) search only, then the shared finalize pipeline.
    /// `top_k <= 0` → `config.lexical_top_k`; an empty or whitespace-only
    /// query returns `Ok(empty)` (the leg's contract).
    pub fn lexical_search(
        &self,
        query: &str,
        top_k: i32,
        domain: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let top_k = if top_k > 0 {
            top_k
        } else {
            self.config.lexical_top_k
        };
        let hits = self.lexical.search(query, top_k as i64, domain)?;
        self.finalize(standalone_results(hits, SourceType::Lexical), top_k)
    }

    /// Semantic (vector) search only, then the shared finalize pipeline.
    /// `top_k <= 0` → `config.semantic_top_k`; an empty or whitespace-only
    /// query returns `Ok(empty)` (the leg's contract).
    pub fn semantic_search(
        &self,
        query: &str,
        top_k: i32,
        domain: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let top_k = if top_k > 0 {
            top_k
        } else {
            self.config.semantic_top_k
        };
        let hits = self.semantic.search(query, top_k as i64, domain)?;
        self.finalize(standalone_results(hits, SourceType::Semantic), top_k)
    }

    /// Fuse the leg hits with RRF over the full pool and run the finalize
    /// pipeline (design D5).
    fn fuse_and_finalize(
        &self,
        lexical: &[LexicalHit],
        semantic: &[SemanticHit],
        top_k: i32,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let fused =
            reciprocal_rank_fusion(lexical, semantic, self.config.rrf_k, self.fusion_pool());
        self.finalize(fused, top_k)
    }

    /// RRF fusion pool size: `max(lexical_top_k, semantic_top_k)`. Each
    /// leg already truncates to its own top-K, so this
    /// is a defensive cap on the fused candidate set.
    fn fusion_pool(&self) -> i32 {
        self.config.lexical_top_k.max(self.config.semantic_top_k)
    }

    /// The shared post-search pipeline (design D5): enrich → rerank →
    /// truncate to topK → graph expand (non-fatal). Domain filtering
    /// already happened inside the sub-searches, so there is no
    /// post-fusion filter.
    fn finalize(
        &self,
        mut results: Vec<SearchResult>,
        top_k: i32,
    ) -> Result<Vec<SearchResult>, SearchError> {
        if results.is_empty() {
            return Ok(results);
        }
        self.enricher.enrich(&mut results)?;
        self.reranker.rerank(&mut results);
        if top_k > 0 {
            results.truncate(top_k as usize);
        }
        if let Some(expander) = &self.expander {
            // Non-fatal by contract (design D8): failures are warnings and
            // the results are returned without graph context.
            expander.expand(&mut results);
        }
        Ok(results)
    }
}

/// The search contract (crate root [`crate::Searcher`]) for the hybrid
/// pipeline: the three entry points delegate to the inherent methods.
impl Searcher for HybridSearcher<'_> {
    fn hybrid_search(
        &self,
        query: &str,
        top_k: i32,
        domain: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        self.hybrid_search(query, top_k, domain)
    }

    fn lexical_search(
        &self,
        query: &str,
        top_k: i32,
        domain: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        self.lexical_search(query, top_k, domain)
    }

    fn semantic_search(
        &self,
        query: &str,
        top_k: i32,
        domain: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        self.semantic_search(query, top_k, domain)
    }
}

/// The common shape of the two raw hit types (identical fields, separate
/// types for the two legs).
trait RawHit {
    /// Chunk row id.
    fn chunk_id(&self) -> i64;
    /// The chunk text (the pure byte-offset slice).
    fn chunk_text(&self) -> &str;
    /// The chunk's metadata bag as raw JSON, if any.
    fn metadata_json(&self) -> Option<&str>;
    /// Owning document id.
    fn document_id(&self) -> i64;
    /// Position of the chunk within its document.
    fn sequence_num(&self) -> i64;
    /// Start offset in the original text, if any.
    fn start_offset(&self) -> Option<i64>;
    /// End offset in the original text, if any.
    fn end_offset(&self) -> Option<i64>;
    /// Raw score, lower is better (FTS5 bm25 / cosine distance).
    fn score(&self) -> f64;
}

impl RawHit for LexicalHit {
    fn chunk_id(&self) -> i64 {
        self.chunk_id
    }
    fn chunk_text(&self) -> &str {
        &self.chunk_text
    }
    fn metadata_json(&self) -> Option<&str> {
        self.metadata_json.as_deref()
    }
    fn document_id(&self) -> i64 {
        self.document_id
    }
    fn sequence_num(&self) -> i64 {
        self.sequence_num
    }
    fn start_offset(&self) -> Option<i64> {
        self.start_offset
    }
    fn end_offset(&self) -> Option<i64> {
        self.end_offset
    }
    fn score(&self) -> f64 {
        self.score
    }
}

impl RawHit for SemanticHit {
    fn chunk_id(&self) -> i64 {
        self.chunk_id
    }
    fn chunk_text(&self) -> &str {
        &self.chunk_text
    }
    fn metadata_json(&self) -> Option<&str> {
        self.metadata_json.as_deref()
    }
    fn document_id(&self) -> i64 {
        self.document_id
    }
    fn sequence_num(&self) -> i64 {
        self.sequence_num
    }
    fn start_offset(&self) -> Option<i64> {
        self.start_offset
    }
    fn end_offset(&self) -> Option<i64> {
        self.end_offset
    }
    fn score(&self) -> f64 {
        self.score
    }
}

/// Map raw leg hits to pre-enrichment results for a standalone leg: the
/// score is inverted (lower-is-better → higher-is-better), 1-based ranks
/// are assigned in leg order,
/// and the chunk's metadata bag is parsed from `metadata_json` (NULL or
/// malformed → an empty bag, design D5). The enrichment slots
/// (`document_path`, `metadata`, `entities`) start empty — the finalize
/// pipeline fills them.
fn standalone_results<H: RawHit>(hits: Vec<H>, source: SourceType) -> Vec<SearchResult> {
    hits.into_iter()
        .enumerate()
        .map(|(position, hit)| SearchResult {
            chunk_id: hit.chunk_id(),
            chunk_text: hit.chunk_text().to_owned(),
            chunk_metadata: chunk_metadata_bag(hit.metadata_json()),
            document_id: hit.document_id(),
            sequence_num: hit.sequence_num(),
            start_offset: hit.start_offset(),
            end_offset: hit.end_offset(),
            document_path: String::new(), // filled by the enricher (design D5)
            score: invert_score(hit.score()),
            rank: position + 1,
            source_type: source.as_str().to_owned(),
            metadata: Map::new(),
            entities: Vec::new(),
        })
        .collect()
}

/// Convert a lower-is-better score (FTS5 bm25, cosine distance) to
/// higher-is-better: the reciprocal, with a zero
/// score mapping to [`f64::MAX`]. Negative bm25 values stay negative and
/// order-preserving (a more negative bm25 is the better match and inverts
/// to the higher value).
fn invert_score(score: f64) -> f64 {
    if score == 0.0 { f64::MAX } else { 1.0 / score }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::atomic::{AtomicUsize, Ordering};

    use config::preset::GraphConfig;
    use db::test_util::in_memory_db;
    use db::{ChunkDao, ChunkEntityDao, ConnectionOrTx, Db, DocumentDao, FactDao};
    use embedding::{EmbeddingError, EmbeddingProvider};
    use graph::Graph;
    use vectors::{VectorIndex, VectorsError};

    use super::*;

    // ── fixtures ────────────────────────────────────────────────────────

    /// Base config: both legs on, k=20, leg top-Ks 20, final top-K 10.
    fn search_config() -> SearchConfig {
        SearchConfig {
            rrf_k: 20,
            lexical_top_k: 20,
            semantic_top_k: 20,
            final_top_k: 10,
            enable_lexical: true,
            enable_semantic: true,
            ..SearchConfig::default()
        }
    }

    /// Canned embedding provider: one fixed vector per call, or a failure;
    /// records the call count.
    struct MockProvider {
        embedding: Vec<f32>,
        fail: bool,
        calls: AtomicUsize,
    }

    impl EmbeddingProvider for MockProvider {
        fn generate_embeddings(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                return Err(EmbeddingError::Model("mock provider failure".to_string()));
            }
            Ok(vec![self.embedding.clone(); texts.len()])
        }

        fn vector_dim(&self) -> usize {
            self.embedding.len()
        }

        fn name(&self) -> &'static str {
            "mock"
        }
    }

    /// Canned index: fixed hits in distance order.
    struct MockIndex {
        hits: Vec<(u32, f32)>,
    }

    impl VectorIndex for MockIndex {
        fn search(&self, _query: &[f32], _k: usize) -> Result<Vec<(u32, f32)>, VectorsError> {
            Ok(self.hits.clone())
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

    fn mock_provider(embedding: Vec<f32>, fail: bool) -> MockProvider {
        MockProvider {
            embedding,
            fail,
            calls: AtomicUsize::new(0),
        }
    }

    fn mock_index(hits: Vec<(u32, f32)>) -> MockIndex {
        MockIndex { hits }
    }

    /// Create a document and return its id.
    fn seed_doc(db: &Db, source_type: &str, path: &str, metadata_json: Option<&str>) -> i64 {
        db.exec_tx(|tx| {
            let documents = DocumentDao::new(ConnectionOrTx::Transaction(&*tx));
            documents.create(source_type, path, metadata_json, None)
        })
        .expect("seed document commits")
    }

    /// Insert a chunk and return its id.
    fn seed_chunk(db: &Db, doc_id: i64, text: &str, seq: i64) -> i64 {
        db.exec_tx(|tx| {
            let chunks = ChunkDao::new(ConnectionOrTx::Transaction(&*tx));
            chunks.create(doc_id, text, seq, None, None)
        })
        .expect("seed chunk commits")
    }

    /// Build the full searcher on a pooled connection with the given
    /// collaborators and run `f` with it. `expander` is the optional
    /// (graph, graph config) pair for the expansion stage.
    fn with_searcher<T>(
        db: &Db,
        config: SearchConfig,
        provider: &MockProvider,
        index: &MockIndex,
        expander: Option<(&Graph, &GraphConfig)>,
        f: impl FnOnce(&HybridSearcher<'_>) -> T,
    ) -> T {
        db.with_conn(|conn| {
            let chunks = ChunkDao::new(ConnectionOrTx::Connection(conn));
            let documents = DocumentDao::new(ConnectionOrTx::Connection(conn));
            let chunk_entities = ChunkEntityDao::new(ConnectionOrTx::Connection(conn));
            let lexical = LexicalSearcher::new(&chunks);
            let semantic = SemanticSearcher::new(&chunks, &documents, provider, index);
            let enricher = Enricher::new(&documents, &chunk_entities);
            let reranker = Reranker::new(Some(&config));
            let facts = FactDao::new(ConnectionOrTx::Connection(conn));
            let expander = expander
                .map(|(graph, graph_config)| GraphExpander::new(graph, &facts, graph_config));
            f(&HybridSearcher::new(
                config, lexical, semantic, enricher, reranker, expander,
            ))
        })
        .expect("connection checkout")
    }

    /// Chunk ids of `results` in result order.
    fn ids(results: &[SearchResult]) -> Vec<i64> {
        results.iter().map(|result| result.chunk_id).collect()
    }

    // ── hybrid entry point ───────────────────────────────────────────────

    // The fusion pool is max(lexical_top_k, semantic_top_k): it caps the
    // fused candidate set when the legs together return more.
    #[test]
    fn hybrid_fusion_pool_is_max_of_leg_tops() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "markdown", "/docs/a.md", None);
        // Five lexical-only chunks (all match "alpha") and five
        // semantic-only chunks (no "alpha").
        let lex: Vec<i64> = (0..5)
            .map(|i| seed_chunk(&db, doc, &format!("alpha lex {i}"), i as i64))
            .collect();
        let sem: Vec<i64> = (0..5)
            .map(|i| seed_chunk(&db, doc, &format!("beta sem {i}"), i as i64 + 5))
            .collect();
        let provider = mock_provider(vec![1.0], false);
        let index = mock_index(sem.iter().map(|id| (*id as u32, 0.1)).collect());

        // max(3, 5): 3 lexical + 5 semantic = 8 candidates, capped to 5.
        let mut config = search_config();
        config.lexical_top_k = 3;
        config.semantic_top_k = 5;
        with_searcher(&db, config, &provider, &index, None, |searcher| {
            assert_eq!(searcher.fusion_pool(), 5, "max(3, 5)");
            let results = searcher.hybrid_search("alpha", 0, None).unwrap();
            assert_eq!(results.len(), 5, "the fused pool is capped to max(3, 5)");
            // The 3 lexical survivors are the top-bm25 "alpha" chunks.
            let fused_ids = ids(&results);
            assert!(
                fused_ids
                    .iter()
                    .all(|id| lex.contains(id) || sem.contains(id))
            );
        });

        // max(5, 3): symmetric.
        let mut config = search_config();
        config.lexical_top_k = 5;
        config.semantic_top_k = 3;
        let index = mock_index(sem.iter().map(|id| (*id as u32, 0.1)).collect());
        with_searcher(&db, config, &provider, &index, None, |searcher| {
            assert_eq!(searcher.fusion_pool(), 5, "max(5, 3)");
            let results = searcher.hybrid_search("alpha", 0, None).unwrap();
            assert_eq!(results.len(), 5, "the fused pool is capped to max(5, 3)");
        });
    }

    // ── pure helpers ─────────────────────────────────────────────────────

    // Score inversion: 0 → f64::MAX, otherwise the reciprocal.
    #[test]
    fn invert_score_reciprocal() {
        assert_eq!(invert_score(0.0), f64::MAX);
        assert_eq!(invert_score(2.0), 0.5);
        assert_eq!(invert_score(-4.0), -0.25);
    }

    // Standalone leg mapping: inverted scores, 1-based ranks in leg order,
    // empty enrichment slots and an empty chunk metadata bag (no
    // metadata_json seeded).
    #[test]
    fn standalone_results_maps_hits() {
        let hits = vec![
            LexicalHit {
                chunk_id: 7,
                chunk_text: "t7".to_string(),
                metadata_json: None,
                document_id: 1,
                sequence_num: 2,
                start_offset: Some(1),
                end_offset: Some(3),
                score: -2.0,
            },
            LexicalHit {
                chunk_id: 3,
                chunk_text: "t3".to_string(),
                metadata_json: Some(r#"{"section_title":"T3"}"#.to_owned()),
                document_id: 1,
                sequence_num: 0,
                start_offset: None,
                end_offset: None,
                score: -1.0,
            },
        ];
        let results = standalone_results(hits, SourceType::Lexical);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].chunk_id, 7);
        assert_eq!(results[0].chunk_text, "t7");
        assert_eq!(results[0].start_offset, Some(1));
        assert_eq!(results[0].end_offset, Some(3));
        assert_eq!(results[0].score, -0.5, "1/-2.0");
        assert_eq!(results[1].score, -1.0, "1/-1.0");
        assert!(
            results[0].score > results[1].score,
            "the better (lower) raw score inverts higher"
        );
        assert_eq!(results[0].rank, 1);
        assert_eq!(results[1].rank, 2);
        assert_eq!(results[0].source_type, "lexical");
        assert!(results[0].metadata.is_empty());
        assert!(results[0].entities.is_empty());
        assert!(results[0].document_path.is_empty());
        // chunk_metadata: NULL → empty bag, JSON → the parsed bag.
        assert!(results[0].chunk_metadata.is_empty(), "NULL → empty bag");
        assert_eq!(
            results[1].chunk_metadata["section_title"],
            serde_json::json!("T3"),
            "the parsed bag lands on the result"
        );
    }

    // The result `text` field (chunk_text) carries the chunk's pure
    // chunk_text (the byte-offset slice), not search_text: a seeded chunk
    // with distinct texts matches on the breadcrumb term (the FTS index is
    // over search_text) and returns the pure body
    // (chunk-metadata-persistence design D5).
    #[test]
    fn result_text_carries_chunk_text() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "markdown", "/docs/a.md", None);
        let chunk_id = db
            .exec_tx(|tx| {
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
        let provider = mock_provider(vec![1.0], false);
        let index = mock_index(Vec::new());

        with_searcher(&db, search_config(), &provider, &index, None, |searcher| {
            let results = searcher.lexical_search("atlas", 10, None).unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].chunk_id, chunk_id);
            assert_eq!(
                results[0].chunk_text, "zebra stripes",
                "the result text field carries the pure chunk_text, not search_text"
            );
        });
    }

    // The result carries the chunk's metadata bag: a seeded chunk with a
    // metadata_json returns the parsed bag in chunk_metadata; a chunk
    // without one returns an empty bag (chunk-metadata-persistence design
    // D5).
    #[test]
    fn result_carries_chunk_metadata_bag() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "markdown", "/docs/a.md", None);
        let with_bag = db
            .exec_tx(|tx| {
                let chunks = ChunkDao::new(ConnectionOrTx::Transaction(&*tx));
                chunks.create_with_search_text(
                    doc,
                    "zebra stripes",
                    "Atlas Guide\n\nzebra stripes",
                    Some(r#"{"section_title":"Guide","breadcrumb":"Atlas Guide"}"#),
                    0,
                    None,
                    None,
                )
            })
            .expect("seed chunk commits");
        let without_bag = db
            .exec_tx(|tx| {
                let chunks = ChunkDao::new(ConnectionOrTx::Transaction(&*tx));
                chunks.create_with_search_text(
                    doc,
                    "zebra spots",
                    "Atlas Guide\n\nzebra spots",
                    None,
                    1,
                    None,
                    None,
                )
            })
            .expect("seed chunk commits");
        let provider = mock_provider(vec![1.0], false);
        let index = mock_index(Vec::new());

        with_searcher(&db, search_config(), &provider, &index, None, |searcher| {
            let results = searcher.lexical_search("zebra", 10, None).unwrap();
            assert_eq!(results.len(), 2);

            let bagged = results.iter().find(|r| r.chunk_id == with_bag).unwrap();
            assert_eq!(
                bagged.chunk_metadata,
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
                    r#"{"section_title":"Guide","breadcrumb":"Atlas Guide"}"#
                )
                .unwrap(),
                "the chunk's bag is parsed onto the result"
            );

            let bare = results.iter().find(|r| r.chunk_id == without_bag).unwrap();
            assert!(
                bare.chunk_metadata.is_empty(),
                "NULL metadata_json → an empty bag"
            );
        });
    }
}
