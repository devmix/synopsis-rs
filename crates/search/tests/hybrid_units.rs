//! Relocated unit tests for the hybrid search pipeline (task 1.7).
//!
//! Extracted from the inline `#[cfg(test)]` module in `src/hybrid.rs` to
//! shrink the source file. They exercise the PUBLIC crate-root API against
//! an in-memory SQLite database (`db::test_util::in_memory_db`) with the
//! model-level seams mocked: a canned embedding provider and a canned
//! search-only index.
//!
//! The three tests that reach private production items (`invert_score`,
//! `standalone_results`, `HybridSearcher::fusion_pool`) remain inline in
//! `src/hybrid.rs` (design D7). The fixtures shared with that stay-inline
//! test (`hybrid_fusion_pool_is_max_of_leg_tops`) are copied here (design D4).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use config::preset::{GraphConfig, SearchConfig};
use db::test_util::in_memory_db;
use db::{
    ChunkDao, ChunkEntityDao, ConnectionOrTx, Db, DocumentDao, Entity, EntityDao, Fact, FactDao,
};
use embedding::{EmbeddingError, EmbeddingProvider};
use graph::Graph;
use search::{
    Enricher, GraphExpander, HybridSearcher, LexicalSearcher, Reranker, SearchError, SearchResult,
    Searcher, SemanticSearcher,
};
use vectors::{VectorIndex, VectorsError};

// ── fixtures ────────────────────────────────────────────────────────────────

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

/// Seed an entity row and return its id.
fn seed_entity(db: &Db, entity_type: &str, name: &str) -> i64 {
    db.exec_tx(|tx| {
        let entities = EntityDao::new(ConnectionOrTx::Transaction(&*tx));
        entities.create(entity_type, name, "hr", None, None, None)
    })
    .expect("seed entity commits")
}

/// Link a chunk to an entity (the enricher's batch source).
fn link_chunk(db: &Db, chunk_id: i64, entity_id: i64) {
    db.with_conn(|conn| {
        let links = ChunkEntityDao::new(ConnectionOrTx::Connection(conn));
        links.link(chunk_id, entity_id)
    })
    .expect("connection checkout")
    .expect("chunk-entity link commits");
}

/// Seed an approved fact row (the expander's batch source).
fn seed_fact(db: &Db, subject: i64, predicate: &str, object: i64) {
    db.exec_tx(|tx| {
        let facts = FactDao::new(ConnectionOrTx::Transaction(&*tx));
        facts.create(Some(subject), predicate, Some(object), "", None, None, None)
    })
    .expect("seed fact commits");
}

/// A graph node row (the expander's BFS input).
fn entity_row(id: i64, name: &str) -> Entity {
    Entity {
        id,
        entity_type: "PERSON".into(),
        name: name.into(),
        domain: "hr".into(),
        description: None,
        confidence: None,
        metadata_json: None,
        created_at: String::new(),
    }
}

/// An approved fact row (the expander's batch source, in-graph copy).
fn fact_row(id: i64, subject: i64, predicate: &str, object: i64) -> Fact {
    Fact {
        id,
        subject_entity_id: Some(subject),
        predicate: predicate.into(),
        object_entity_id: Some(object),
        domain: String::new(),
        metadata_json: None,
        status: "approved".into(),
        valid_from: None,
        valid_to: None,
        weight: 0,
        created_at: String::new(),
        updated_at: String::new(),
    }
}

fn graph_config(max_depth: i32, max_nodes: i32) -> GraphConfig {
    GraphConfig {
        max_depth,
        max_nodes,
        ..Default::default()
    }
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
        let expander =
            expander.map(|(graph, graph_config)| GraphExpander::new(graph, &facts, graph_config));
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

// ── hybrid entry point ───────────────────────────────────────────────────────

// Empty and whitespace-only queries: no legs run, Ok(empty).
#[test]
fn hybrid_empty_query_returns_empty_without_leg_calls() {
    let db = in_memory_db();
    let doc = seed_doc(&db, "markdown", "/docs/a.md", None);
    seed_chunk(&db, doc, "alpha beta", 0);
    let provider = mock_provider(vec![1.0], false);
    let index = mock_index(Vec::new());

    with_searcher(&db, search_config(), &provider, &index, None, |searcher| {
        assert!(searcher.hybrid_search("", 10, None).unwrap().is_empty());
        assert!(
            searcher
                .hybrid_search("   \t ", 10, None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            provider.calls.load(Ordering::Relaxed),
            0,
            "no embedding call for an empty query"
        );
    });
}

// Both legs disabled: Ok(empty) for a matching query.
#[test]
fn hybrid_both_legs_disabled_returns_empty() {
    let db = in_memory_db();
    let doc = seed_doc(&db, "markdown", "/docs/a.md", None);
    seed_chunk(&db, doc, "alpha beta", 0);
    let mut config = search_config();
    config.enable_lexical = false;
    config.enable_semantic = false;
    let provider = mock_provider(vec![1.0], false);
    let index = mock_index(Vec::new());

    with_searcher(&db, config, &provider, &index, None, |searcher| {
        assert!(
            searcher
                .hybrid_search("alpha", 10, None)
                .unwrap()
                .is_empty(),
            "both legs disabled → empty, not an error"
        );
        assert_eq!(
            provider.calls.load(Ordering::Relaxed),
            0,
            "the disabled semantic leg must not embed"
        );
    });
}

// Both legs fail: hard error carrying both causes (design D9).
#[test]
fn hybrid_both_legs_failing_is_hard_error_with_both_causes() {
    let db = in_memory_db();
    let doc = seed_doc(&db, "markdown", "/docs/a.md", None);
    // A chunk must exist: on an empty FTS index SQLite short-circuits
    // the MATCH and the malformed query would not fail.
    seed_chunk(&db, doc, "alpha beta", 0);
    let provider = mock_provider(vec![1.0], true); // embedding failure
    let index = mock_index(Vec::new());

    with_searcher(&db, search_config(), &provider, &index, None, |searcher| {
        // A malformed FTS5 MATCH fails the lexical leg; the failing
        // provider fails the semantic leg.
        match searcher
            .hybrid_search(r#"unclosed "phrase"#, 10, None)
            .unwrap_err()
        {
            SearchError::BothSubSearchesFailed { lexical, semantic } => {
                assert!(lexical.contains("database"), "lexical cause: {lexical}");
                assert!(semantic.contains("embedding"), "semantic cause: {semantic}");
            }
            other => panic!("expected BothSubSearchesFailed, got: {other:?}"),
        }
    });
}

// One leg fails: degrade to the survivor (design D9).
#[test]
fn hybrid_lexical_failure_degrades_to_semantic() {
    let db = in_memory_db();
    let doc = seed_doc(&db, "markdown", "/docs/a.md", None);
    let c1 = seed_chunk(&db, doc, "alpha one", 0);
    let c2 = seed_chunk(&db, doc, "beta two", 1);
    let provider = mock_provider(vec![1.0], false);
    let index = mock_index(vec![(c1 as u32, 0.1), (c2 as u32, 0.2)]);

    with_searcher(&db, search_config(), &provider, &index, None, |searcher| {
        let results = searcher
            .hybrid_search(r#"unclosed "phrase"#, 10, None)
            .unwrap();
        assert_eq!(results.len(), 2, "the surviving semantic leg's hits");
        assert_eq!(ids(&results), vec![c1, c2], "semantic leg order preserved");
        for result in &results {
            assert_eq!(result.source_type, "semantic+markdown");
            assert_eq!(
                result.document_path, "/docs/a.md",
                "finalize enriched the pool"
            );
        }
    });
}

#[test]
fn hybrid_semantic_failure_degrades_to_lexical() {
    let db = in_memory_db();
    let doc = seed_doc(&db, "markdown", "/docs/a.md", None);
    seed_chunk(&db, doc, "alpha one", 0);
    seed_chunk(&db, doc, "alpha two", 1);
    let provider = mock_provider(vec![1.0], true);
    let index = mock_index(Vec::new());

    with_searcher(&db, search_config(), &provider, &index, None, |searcher| {
        let results = searcher.hybrid_search("alpha", 10, None).unwrap();
        assert_eq!(results.len(), 2, "the surviving lexical leg's hits");
        for result in &results {
            assert_eq!(result.source_type, "lexical+markdown");
            assert_eq!(result.document_path, "/docs/a.md");
        }
    });
}

// Both legs succeed: RRF fusion marks the shared chunk hybrid and
// ranks it first.
#[test]
fn hybrid_fuses_both_legs_with_source_types() {
    let db = in_memory_db();
    let doc = seed_doc(&db, "markdown", "/docs/a.md", None);
    let c1 = seed_chunk(&db, doc, "alpha alpha one", 0); // lexical rank 1 + semantic rank 2
    let c2 = seed_chunk(&db, doc, "alpha gamma two", 1); // lexical rank 2 only
    let c3 = seed_chunk(&db, doc, "delta epsilon three", 2); // semantic rank 1 only
    let provider = mock_provider(vec![1.0], false);
    let index = mock_index(vec![(c3 as u32, 0.1), (c1 as u32, 0.2)]);

    with_searcher(&db, search_config(), &provider, &index, None, |searcher| {
        let results = searcher.hybrid_search("alpha", 10, None).unwrap();
        assert_eq!(results.len(), 3);
        let by_id: HashMap<i64, &SearchResult> = results
            .iter()
            .map(|result| (result.chunk_id, result))
            .collect();
        assert_eq!(
            by_id[&c1].source_type, "hybrid+markdown",
            "chunk in both legs"
        );
        assert_eq!(by_id[&c2].source_type, "lexical+markdown");
        assert_eq!(by_id[&c3].source_type, "semantic+markdown");
        assert_eq!(
            results[0].chunk_id, c1,
            "the chunk in both legs ranks first"
        );
        assert_eq!(results[0].rank, 1);
    });
}

// Finalize order (design D5): enrich → rerank → truncate → expand.
//
// Pre-rerank the order is the (mock) index order c1, c2, c3. The
// rerank multiplies c2 (deprecated) by 0.2 and c3 (official) by 1.5,
// so AFTER the rerank the topK=2 truncation keeps c1 and c3 and drops
// the deprecated c2. Truncating BEFORE the rerank would keep c1 and
// c2 instead — the survivor set is the observable of the ordering.
// The expander is present: the surviving c1's linked entity gains
// `related_entities` (expand runs on the truncated pool).
#[test]
fn hybrid_finalize_enriches_reranks_truncates_and_expands() {
    let db = in_memory_db();
    let doc_plain = seed_doc(&db, "tutorial", "/docs/plain.md", Some("{}"));
    let doc_deprecated = seed_doc(
        &db,
        "tutorial",
        "/docs/deprecated.md",
        Some(r#"{"is_deprecated":true}"#),
    );
    let doc_official = seed_doc(
        &db,
        "policy",
        "/docs/official.md",
        Some(r#"{"is_official":true}"#),
    );
    let c1 = seed_chunk(&db, doc_plain, "alpha one", 0);
    let c2 = seed_chunk(&db, doc_deprecated, "alpha two", 1);
    let c3 = seed_chunk(&db, doc_official, "alpha three", 2);

    // Graph context for the surviving chunk's entity.
    let e1 = seed_entity(&db, "PERSON", "Alice");
    let e2 = seed_entity(&db, "PERSON", "Bob");
    seed_fact(&db, e1, "collaborates_with", e2);
    link_chunk(&db, c1, e1);
    let graph = Graph::from_rows(
        vec![entity_row(e1, "Alice"), entity_row(e2, "Bob")],
        vec![fact_row(1, e1, "collaborates_with", e2)],
        Vec::new(),
    );

    // Only the semantic leg runs (deterministic index order), final
    // top-K 2, caller passes top_k=0 → the default kicks in.
    let mut config = search_config();
    config.enable_lexical = false;
    config.final_top_k = 2;
    let provider = mock_provider(vec![1.0], false);
    let index = mock_index(vec![(c1 as u32, 0.1), (c2 as u32, 0.2), (c3 as u32, 0.3)]);
    let gconfig = graph_config(3, 100);

    with_searcher(
        &db,
        config,
        &provider,
        &index,
        Some((&graph, &gconfig)),
        |searcher| {
            let results = searcher.hybrid_search("query", 0, None).unwrap();

            // Truncate + rerank: 3 candidates → 2, the deprecated chunk
            // dropped, the official chunk promoted into the survivor set.
            assert_eq!(ids(&results), vec![c1, c3]);
            assert_eq!(results[0].rank, 1);
            assert_eq!(results[1].rank, 2);

            // Rerank math (base fusion scores 0.85 / 0.15 for ranks 1/3,
            // uniform ×1.2 freshness, then ×1.0 / ×1.5): c3 = 0.15·1.2·1.5.
            assert!(
                (results[1].score - 0.27).abs() <= 1e-4,
                "got {}",
                results[1].score
            );
            assert!(
                results[0].score > results[1].score,
                "plain chunk keeps the lead"
            );

            // Enrichment landed: paths and merged source types.
            assert_eq!(results[0].document_path, "/docs/plain.md");
            assert_eq!(results[1].document_path, "/docs/official.md");
            assert_eq!(results[1].source_type, "semantic+policy");

            // Expand (last stage, on the truncated pool): c1's entity has
            // the graph context; the dropped c2 is not in the pool at all.
            let related = results[0]
                .metadata
                .get("related_entities")
                .expect("related_entities present on the surviving chunk")
                .as_array()
                .expect("an array");
            assert_eq!(related.len(), 1);
            assert_eq!(related[0]["entity_id"], e1);
            let edges = related[0]
                .get("edges")
                .expect("edges present")
                .as_array()
                .expect("array");
            assert_eq!(edges[0]["source_id"], e1);
            assert_eq!(edges[0]["target_id"], e2);
        },
    );
}

// top_k <= 0 falls back to config.final_top_k (hybrid entry point).
#[test]
fn hybrid_top_k_defaults_to_final_top_k() {
    let db = in_memory_db();
    let doc = seed_doc(&db, "markdown", "/docs/a.md", None);
    for i in 0..4 {
        seed_chunk(&db, doc, &format!("alpha hit {i}"), i as i64);
    }
    let mut config = search_config();
    config.final_top_k = 2;
    config.enable_semantic = false;
    let provider = mock_provider(vec![1.0], false);
    let index = mock_index(Vec::new());

    with_searcher(&db, config, &provider, &index, None, |searcher| {
        for top_k in [0, -1] {
            let results = searcher.hybrid_search("alpha", top_k, None).unwrap();
            assert_eq!(results.len(), 2, "top_k {top_k} → final_top_k 2");
        }
        let explicit = searcher.hybrid_search("alpha", 3, None).unwrap();
        assert_eq!(explicit.len(), 3, "an explicit top_k wins over the default");
    });
}

// The domain filter is passed through to both legs (sub-search
// contract, design D3/D5: filtering never happens after fusion).
#[test]
fn hybrid_passes_domain_to_both_legs() {
    let db = in_memory_db();
    let hr = seed_doc(&db, "markdown", "/docs/hr.md", Some(r#"{"domain":"hr"}"#));
    let eng = seed_doc(
        &db,
        "markdown",
        "/docs/eng.md",
        Some(r#"{"domain":"engineering"}"#),
    );
    let hr_chunks: Vec<i64> = (0..2)
        .map(|i| seed_chunk(&db, hr, &format!("alpha hr {i}"), i as i64))
        .collect();
    let eng_chunks: Vec<i64> = (0..2)
        .map(|i| seed_chunk(&db, eng, &format!("alpha eng {i}"), i as i64 + 2))
        .collect();
    let provider = mock_provider(vec![1.0], false);
    let index = mock_index(
        hr_chunks
            .iter()
            .chain(eng_chunks.iter())
            .enumerate()
            .map(|(i, id)| (*id as u32, i as f32))
            .collect(),
    );

    with_searcher(&db, search_config(), &provider, &index, None, |searcher| {
        let results = searcher.hybrid_search("alpha", 10, Some("hr")).unwrap();
        let mut fused = ids(&results);
        fused.sort_unstable();
        assert_eq!(fused, hr_chunks, "only in-domain chunks survive both legs");

        let all = searcher.hybrid_search("alpha", 10, None).unwrap();
        assert_eq!(all.len(), 4, "no domain → every chunk");
    });
}

// ── standalone legs ──────────────────────────────────────────────────────────

// Standalone lexical: top_k default, score inversion, enrichment.
#[test]
fn lexical_search_standalone_inverts_scores_and_finalizes() {
    let db = in_memory_db();
    let doc = seed_doc(&db, "markdown", "/docs/a.md", None);
    let c1 = seed_chunk(&db, doc, "alpha alpha one", 0); // best bm25 (more matches)
    let c2 = seed_chunk(&db, doc, "alpha two", 1);
    let mut config = search_config();
    config.lexical_top_k = 2;
    let provider = mock_provider(vec![1.0], false);
    let index = mock_index(Vec::new());

    with_searcher(&db, config, &provider, &index, None, |searcher| {
        // top_k=0 → lexical_top_k=2.
        let results = searcher.lexical_search("alpha", 0, None).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].chunk_id, c1, "the better bm25 match ranks first");
        assert_eq!(results[1].chunk_id, c2);
        // Inversion: FTS5 bm25 is lower-is-better; the inverted score
        // is higher for the better match.
        assert!(
            results[0].score > results[1].score,
            "scores must be inverted"
        );
        // Enrichment: merged source type and document path.
        assert_eq!(results[0].source_type, "lexical+markdown");
        assert_eq!(results[0].document_path, "/docs/a.md");
        assert_eq!(results[0].rank, 1);
        assert_eq!(results[1].rank, 2);

        // Empty query → empty (the leg's contract).
        assert!(searcher.lexical_search("", 10, None).unwrap().is_empty());
    });
}

// Standalone semantic: index order, exact score inversion, enrichment.
#[test]
fn semantic_search_standalone_inverts_scores_and_finalizes() {
    let db = in_memory_db();
    let doc = seed_doc(&db, "markdown", "/docs/a.md", None);
    let c1 = seed_chunk(&db, doc, "first chunk", 0);
    let c2 = seed_chunk(&db, doc, "second chunk", 1);
    let provider = mock_provider(vec![1.0], false);
    let index = mock_index(vec![(c2 as u32, 0.1), (c1 as u32, 0.5)]);

    with_searcher(&db, search_config(), &provider, &index, None, |searcher| {
        let results = searcher.semantic_search("query", 10, None).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].chunk_id, c2, "index (distance) order preserved");
        // Inversion (1/0.1 → 10, 1/0.5 → 2) then the shared finalize's
        // rerank: the fresh document gets the ×1.2 boost.
        assert!(
            (results[0].score - 12.0).abs() <= 1e-4,
            "got {}",
            results[0].score
        );
        assert!(
            (results[1].score - 2.4).abs() <= 1e-4,
            "got {}",
            results[1].score
        );
        assert_eq!(results[0].source_type, "semantic+markdown");
        assert_eq!(results[0].document_path, "/docs/a.md");
        assert_eq!(results[0].rank, 1);
        assert_eq!(results[1].rank, 2);

        // top_k=0 → semantic_top_k=20 → both hits.
        assert_eq!(searcher.semantic_search("query", 0, None).unwrap().len(), 2);

        // Empty query → empty, no embedding call.
        assert!(searcher.semantic_search("", 10, None).unwrap().is_empty());
        assert_eq!(
            provider.calls.load(Ordering::Relaxed),
            2,
            "the empty query must not embed"
        );
    });
}

// ── trait contract ───────────────────────────────────────────────────────────

// The Searcher trait is implemented (the object-safe contract for the
// MCP layer).
#[test]
fn hybrid_searcher_is_a_searcher() {
    let db = in_memory_db();
    let doc = seed_doc(&db, "markdown", "/docs/a.md", None);
    seed_chunk(&db, doc, "alpha beta", 0);
    let provider = mock_provider(vec![1.0], false);
    let index = mock_index(Vec::new());

    with_searcher(&db, search_config(), &provider, &index, None, |searcher| {
        let via_trait: &dyn Searcher = searcher;
        assert_eq!(via_trait.hybrid_search("alpha", 10, None).unwrap().len(), 1);
        assert_eq!(
            via_trait.lexical_search("alpha", 10, None).unwrap().len(),
            1
        );
        assert!(
            via_trait
                .semantic_search("nomatch", 10, None)
                .unwrap()
                .is_empty()
        );
    });
}
