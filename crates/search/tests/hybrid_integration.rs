//! End-to-end integration test for the hybrid search pipeline (task 4.7).
//!
//! Unlike the unit tests in `src/`, this test exercises the PUBLIC crate-root
//! API against REAL infrastructure: an in-memory SQLite database with a real
//! FTS5 index (`db::test_util::in_memory_db`, the crates/db test pattern) and
//! documents/chunks/entities seeded through the db DAOs. The only seams
//! mocked are the model-level ones, which the product never exposes:
//!
//! - [`MockEmbedding`] — a deterministic vocabulary-count embedding provider
//!   (bag-of-words over a fixed 6-word vocabulary), so vector distances are
//!   exact and the expected orderings are hand-computable;
//! - [`MemoryIndex`] — a brute-force in-memory L2 [`VectorIndex`] (the
//!   mock-pattern stand-in for the vector engine; ties keep insertion
//!   order via the stable sort).
//!
//! Covered (task 4.7 acceptance): lexical / semantic / hybrid runs through
//! the [`Searcher`] contract with ordering and enrichment assertions
//! (`document_path`, merged `source_type`, attached entities), the domain
//! filter on both legs, the reranker's official boost reordering results,
//! and empty queries returning empty.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;

use config::preset::SearchConfig;
use db::test_util::in_memory_db;
use db::{ChunkDao, ChunkEntityDao, ConnectionOrTx, Db, DocumentDao, EntityDao};
use embedding::{EmbeddingError, EmbeddingProvider};
use search::{
    Enricher, HybridSearcher, LexicalSearcher, Reranker, SearchResult, Searcher, SemanticSearcher,
};
use serde_json::json;
use vectors::{VectorIndex, VectorsError};

// ── test doubles ──────────────────────────────────────────────────────────

/// The fixed vocabulary of [`MockEmbedding`]: a chunk's embedding is the
/// per-word occurrence count over this vocabulary (order = dimension).
const VOCAB: &[&str] = &[
    "benefits",
    "conduct",
    "design",
    "migration",
    "onboarding",
    "policy",
];

/// Deterministic vocabulary-count embedding provider: `embed(text)` counts
/// each vocabulary word in the lowercased tokens of `text`. Similar texts
/// (sharing vocabulary words) get close vectors, so L2 orderings are exact.
struct MockEmbedding;

impl EmbeddingProvider for MockEmbedding {
    fn generate_embeddings(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        Ok(texts.iter().map(|text| embed(text)).collect())
    }

    fn vector_dim(&self) -> usize {
        VOCAB.len()
    }

    fn name(&self) -> &'static str {
        "mock-vocab"
    }
}

/// The vocabulary-count embedding of `text`.
fn embed(text: &str) -> Vec<f32> {
    let lower = text.to_lowercase();
    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_alphabetic())
        .filter(|token| !token.is_empty())
        .collect();
    VOCAB
        .iter()
        .map(|word| tokens.iter().filter(|token| *token == word).count() as f32)
        .collect()
}

/// Brute-force in-memory L2 index (the test's [`VectorIndex`] stand-in).
///
/// `search` sorts by L2 distance ascending with a STABLE sort, so equal
/// distances keep insertion order — that determinism is what pins the
/// expected result orderings below.
struct MemoryIndex {
    dim: usize,
    rows: Mutex<Vec<(u32, Vec<f32>)>>,
}

impl MemoryIndex {
    /// Create an empty index over `dim`-dimensional vectors.
    fn new(dim: usize) -> Self {
        Self {
            dim,
            rows: Mutex::new(Vec::new()),
        }
    }
}

impl VectorIndex for MemoryIndex {
    fn insert(&self, chunk_id: u32, vector: &[f32]) -> Result<(), VectorsError> {
        check_dim(self.dim, vector)?;
        self.rows.lock().unwrap().push((chunk_id, vector.to_vec()));
        Ok(())
    }

    fn insert_batch(&self, rows: &[(u32, &[f32])]) -> Result<(), VectorsError> {
        for (chunk_id, vector) in rows {
            self.insert(*chunk_id, vector)?;
        }
        Ok(())
    }

    fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u32, f32)>, VectorsError> {
        check_dim(self.dim, query)?;
        let mut scored: Vec<(u32, f32)> = self
            .rows
            .lock()
            .unwrap()
            .iter()
            .map(|(id, vector)| (*id, l2(query, vector)))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(k);
        Ok(scored)
    }

    fn delete_by_chunk_ids(&self, chunk_ids: &[u32]) -> Result<(), VectorsError> {
        self.rows
            .lock()
            .unwrap()
            .retain(|(id, _)| !chunk_ids.contains(id));
        Ok(())
    }

    fn chunk_ids(&self) -> Result<Vec<u32>, VectorsError> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .map(|(id, _)| *id)
            .collect())
    }

    fn count(&self) -> Result<u64, VectorsError> {
        Ok(self.rows.lock().unwrap().len() as u64)
    }

    fn build_index(&self) -> Result<(), VectorsError> {
        Ok(())
    }

    fn rebuild(&self, rows: &[(u32, Vec<f32>)]) -> Result<(), VectorsError> {
        *self.rows.lock().unwrap() = rows.to_vec();
        Ok(())
    }
}

fn check_dim(dim: usize, vector: &[f32]) -> Result<(), VectorsError> {
    if vector.len() != dim {
        return Err(VectorsError::DimensionMismatch {
            expected: dim,
            actual: vector.len(),
        });
    }
    Ok(())
}

fn l2(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (x - y) * (x - y))
        .sum::<f32>()
        .sqrt()
}

// ── fixture ───────────────────────────────────────────────────────────────

/// Chunk texts (shared by the db seeding and the index seeding).
const HR_BENEFITS: &str = "Employee benefits policy overview";
const HR_ONBOARDING: &str = "Onboarding checklist for new hires";
const ENG_DESIGN: &str = "System design review guidelines v2";
const ENG_MIGRATION: &str = "Database migration runbook";
const POL_CONDUCT: &str = "Code of conduct policy statement";

/// The integration corpus: three documents across two domains, five chunks.
///
/// Domains live in the document `metadata_json` (`$.domain`), which is what
/// both sub-search legs filter on. The text choices pin the expected
/// orderings:
///
/// - lexical `"policy"` → `HR_BENEFITS` (4 tokens, best bm25) then
///   `POL_CONDUCT` (5 tokens);
/// - lexical `"onboarding OR design"` → `HR_ONBOARDING` (4 tokens) then
///   `ENG_DESIGN` (5 tokens);
/// - semantic `"policy"` (embeds to `[..., 1]`) → distances: `HR_BENEFITS`
///   1.0, `POL_CONDUCT` 1.0, the other three √2 — insertion order breaks
///   the ties.
struct Corpus {
    /// The hr-domain document's chunks: `[HR_BENEFITS, HR_ONBOARDING]`.
    hr: [i64; 2],
    /// The engineering-domain document's chunks: `[ENG_DESIGN, ENG_MIGRATION]`.
    engineering: [i64; 2],
    /// The code-of-conduct chunk (hr domain): `POL_CONDUCT`.
    conduct: i64,
}

impl Corpus {
    /// Every `(chunk_id, chunk_text)` pair in insertion order.
    fn chunks(&self) -> [(i64, &'static str); 5] {
        [
            (self.hr[0], HR_BENEFITS),
            (self.hr[1], HR_ONBOARDING),
            (self.engineering[0], ENG_DESIGN),
            (self.engineering[1], ENG_MIGRATION),
            (self.conduct, POL_CONDUCT),
        ]
    }
}

fn seed_doc(db: &Db, source_type: &str, path: &str, metadata_json: Option<&str>) -> i64 {
    db.exec_tx(|tx| {
        let documents = DocumentDao::new(ConnectionOrTx::Transaction(&*tx));
        documents.create(source_type, path, metadata_json, None)
    })
    .expect("seed document commits")
}

fn seed_chunk(db: &Db, doc_id: i64, text: &str, seq: i64) -> i64 {
    db.exec_tx(|tx| {
        let chunks = ChunkDao::new(ConnectionOrTx::Transaction(&*tx));
        chunks.create(doc_id, text, seq, None, None)
    })
    .expect("seed chunk commits")
}

fn seed_entity(db: &Db, entity_type: &str, name: &str) -> i64 {
    db.exec_tx(|tx| {
        let entities = EntityDao::new(ConnectionOrTx::Transaction(&*tx));
        entities.create(entity_type, name, "hr", None, None, None)
    })
    .expect("seed entity commits")
}

fn link_chunk(db: &Db, chunk_id: i64, entity_id: i64) {
    db.with_conn(|conn| {
        let links = ChunkEntityDao::new(ConnectionOrTx::Connection(conn));
        links.link(chunk_id, entity_id)
    })
    .expect("connection checkout")
    .expect("chunk-entity link commits");
}

/// Seed the corpus documents and chunks (direct DAO inserts, real FTS5 rows).
fn seed_corpus(db: &Db) -> Corpus {
    let hr_doc = seed_doc(
        db,
        "markdown",
        "/docs/hr-policy.md",
        Some(r#"{"domain":"hr"}"#),
    );
    let eng_doc = seed_doc(
        db,
        "pdf",
        "/docs/engineering-design.pdf",
        Some(r#"{"domain":"engineering"}"#),
    );
    let pol_doc = seed_doc(
        db,
        "policy",
        "/docs/code-of-conduct.md",
        Some(r#"{"domain":"hr"}"#),
    );

    let hr1 = seed_chunk(db, hr_doc, HR_BENEFITS, 0);
    let hr2 = seed_chunk(db, hr_doc, HR_ONBOARDING, 1);
    let eng1 = seed_chunk(db, eng_doc, ENG_DESIGN, 0);
    let eng2 = seed_chunk(db, eng_doc, ENG_MIGRATION, 1);
    let pol1 = seed_chunk(db, pol_doc, POL_CONDUCT, 0);

    Corpus {
        hr: [hr1, hr2],
        engineering: [eng1, eng2],
        conduct: pol1,
    }
}

/// Index every corpus chunk under its vocabulary-count embedding.
fn index_corpus(index: &MemoryIndex, corpus: &Corpus) {
    for (chunk_id, text) in corpus.chunks() {
        index.insert(chunk_id as u32, &embed(text)).unwrap();
    }
}

// ── searcher assembly (public API, collaborators injected) ────────────────

/// Base config: both legs on, k=20, leg top-Ks 20, final top-K 10 — the
/// same base the src unit tests use.
fn base_config() -> SearchConfig {
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

/// Assemble the full [`HybridSearcher`] on a pooled connection and run `f`
/// with it (no graph expander: expansion is pinned by the src unit tests).
fn with_searcher<T>(
    db: &Db,
    config: SearchConfig,
    index: &MemoryIndex,
    f: impl FnOnce(&HybridSearcher<'_>) -> T,
) -> T {
    db.with_conn(|conn| {
        let chunks = ChunkDao::new(ConnectionOrTx::Connection(conn));
        let documents = DocumentDao::new(ConnectionOrTx::Connection(conn));
        let chunk_entities = ChunkEntityDao::new(ConnectionOrTx::Connection(conn));
        let lexical = LexicalSearcher::new(&chunks);
        let semantic = SemanticSearcher::new(&chunks, &documents, &MockEmbedding, index);
        let enricher = Enricher::new(&documents, &chunk_entities);
        let reranker = Reranker::new(Some(&config));
        f(&HybridSearcher::new(
            config, lexical, semantic, enricher, reranker, None,
        ))
    })
    .expect("connection checkout")
}

// ── assertion helpers ─────────────────────────────────────────────────────

fn ids(results: &[SearchResult]) -> Vec<i64> {
    results.iter().map(|result| result.chunk_id).collect()
}

fn ranks(results: &[SearchResult]) -> Vec<usize> {
    results.iter().map(|result| result.rank).collect()
}

fn assert_strictly_decreasing(scores: &[f64]) {
    for (higher, lower) in scores.iter().zip(scores.iter().skip(1)) {
        assert!(
            *higher > *lower,
            "scores must strictly decrease: {scores:?}"
        );
    }
}

// ── lexical leg ───────────────────────────────────────────────────────────

// bm25 ordering, score inversion, and the full enrichment (path, merged
// source type, domains, updated_at, attached entities).
#[test]
fn lexical_search_orders_by_bm25_and_enriches() {
    let db = in_memory_db();
    let corpus = seed_corpus(&db);
    let alice = seed_entity(&db, "PERSON", "Alice");
    link_chunk(&db, corpus.hr[0], alice);
    let index = MemoryIndex::new(MockEmbedding.vector_dim());

    with_searcher(&db, base_config(), &index, |searcher| {
        let searcher: &dyn Searcher = searcher;
        let results = searcher.lexical_search("policy", 10, None).unwrap();

        // bm25: both chunks match "policy" once; the shorter document wins.
        assert_eq!(ids(&results), vec![corpus.hr[0], corpus.conduct]);
        assert_eq!(ranks(&results), vec![1, 2]);
        assert!(
            results[0].score > results[1].score,
            "inverted bm25 must be higher for the better match"
        );

        // Enrichment: merged source type, document path, domains, updated_at.
        let (first, second) = (&results[0], &results[1]);
        assert_eq!(first.source_type, "lexical+markdown");
        assert_eq!(second.source_type, "lexical+policy");
        assert_eq!(first.document_path, "/docs/hr-policy.md");
        assert_eq!(second.document_path, "/docs/code-of-conduct.md");
        assert_eq!(first.metadata["domains"], json!(["hr"]));
        assert_eq!(second.metadata["domains"], json!(["hr"]));
        assert!(first.metadata.get("updated_at").is_some());
        assert!(second.metadata.get("updated_at").is_some());

        // Entities attached from the chunk-entity links.
        assert_eq!(first.entities.len(), 1);
        assert_eq!(first.entities[0].name, "Alice");
        assert_eq!(first.entities[0].entity_type, "PERSON");
        assert!(second.entities.is_empty());
    });
}

// Domain filter (SQL-side on the lexical leg): in-domain only, no-match
// empty, case-insensitive.
#[test]
fn lexical_search_domain_filter() {
    let db = in_memory_db();
    let corpus = seed_corpus(&db);
    let index = MemoryIndex::new(MockEmbedding.vector_dim());

    with_searcher(&db, base_config(), &index, |searcher| {
        let searcher: &dyn Searcher = searcher;
        // Matches one hr chunk (HR_ONBOARDING) and one engineering chunk
        // (ENG_DESIGN); the shorter document ranks first.
        let all = searcher
            .lexical_search("onboarding OR design", 10, None)
            .unwrap();
        assert_eq!(ids(&all), vec![corpus.hr[1], corpus.engineering[0]]);

        let hr = searcher
            .lexical_search("onboarding OR design", 10, Some("hr"))
            .unwrap();
        assert_eq!(ids(&hr), vec![corpus.hr[1]]);

        let eng = searcher
            .lexical_search("onboarding OR design", 10, Some("engineering"))
            .unwrap();
        assert_eq!(ids(&eng), vec![corpus.engineering[0]]);

        let none = searcher
            .lexical_search("onboarding OR design", 10, Some("product"))
            .unwrap();
        assert!(none.is_empty(), "unknown domain → empty, not an error");

        // Case-insensitive (both sides normalized).
        let upper = searcher
            .lexical_search("onboarding OR design", 10, Some("HR "))
            .unwrap();
        assert_eq!(ids(&upper), vec![corpus.hr[1]]);
    });
}

// ── semantic leg ──────────────────────────────────────────────────────────

// Index (distance) order preserved through resolution, inversion and the
// shared finalize; enrichment landed on the whole pool.
#[test]
fn semantic_search_preserves_index_order_and_enriches() {
    let db = in_memory_db();
    let corpus = seed_corpus(&db);
    let index = MemoryIndex::new(MockEmbedding.vector_dim());
    index_corpus(&index, &corpus);

    with_searcher(&db, base_config(), &index, |searcher| {
        let searcher: &dyn Searcher = searcher;
        let results = searcher.semantic_search("policy", 10, None).unwrap();

        // Distances: HR_BENEFITS 1.0, POL_CONDUCT 1.0, the rest √2 — the
        // stable sort keeps insertion order inside the ties.
        let expected = vec![
            corpus.hr[0],
            corpus.conduct,
            corpus.hr[1],
            corpus.engineering[0],
            corpus.engineering[1],
        ];
        assert_eq!(ids(&results), expected);
        assert_eq!(ranks(&results), vec![1, 2, 3, 4, 5]);

        // Inverted distances, then the uniform freshness boost: non-increasing.
        let scores: Vec<f64> = results.iter().map(|result| result.score).collect();
        for (higher, lower) in scores.iter().zip(scores.iter().skip(1)) {
            assert!(
                *higher >= *lower,
                "scores must be non-increasing: {scores:?}"
            );
        }

        assert_eq!(results[0].source_type, "semantic+markdown");
        assert_eq!(results[0].document_path, "/docs/hr-policy.md");
        assert_eq!(results[0].metadata["domains"], json!(["hr"]));
        assert!(results[0].metadata.get("updated_at").is_some());
    });
}

// ── hybrid ────────────────────────────────────────────────────────────────

// Both legs run, RRF fusion ranks the shared chunks first, and the finalize
// pipeline enriches the whole pool.
#[test]
fn hybrid_search_fuses_ranks_and_enriches() {
    let db = in_memory_db();
    let corpus = seed_corpus(&db);
    let alice = seed_entity(&db, "PERSON", "Alice");
    link_chunk(&db, corpus.hr[0], alice);
    let index = MemoryIndex::new(MockEmbedding.vector_dim());
    index_corpus(&index, &corpus);

    with_searcher(&db, base_config(), &index, |searcher| {
        let searcher: &dyn Searcher = searcher;
        let results = searcher.hybrid_search("policy", 10, None).unwrap();

        // Lexical: [HR_BENEFITS, POL_CONDUCT]; semantic:
        // [HR_BENEFITS, POL_CONDUCT, HR_ONBOARDING, ENG_DESIGN, ENG_MIGRATION].
        // RRF: the chunk in both legs leads, then the other hybrid chunk,
        // then the semantic-only chunks in index order.
        let expected = vec![
            corpus.hr[0],
            corpus.conduct,
            corpus.hr[1],
            corpus.engineering[0],
            corpus.engineering[1],
        ];
        assert_eq!(ids(&results), expected);
        assert_eq!(ranks(&results), vec![1, 2, 3, 4, 5]);
        let scores: Vec<f64> = results.iter().map(|result| result.score).collect();
        assert_strictly_decreasing(&scores);

        // Merged source types: both legs → hybrid, one leg → that leg.
        assert_eq!(results[0].source_type, "hybrid+markdown");
        assert_eq!(results[1].source_type, "hybrid+policy");
        assert_eq!(results[2].source_type, "semantic+markdown");
        assert_eq!(results[3].source_type, "semantic+pdf");
        assert_eq!(results[4].source_type, "semantic+pdf");

        // Enrichment across the whole pool.
        assert_eq!(results[0].document_path, "/docs/hr-policy.md");
        assert_eq!(results[1].document_path, "/docs/code-of-conduct.md");
        assert_eq!(results[3].document_path, "/docs/engineering-design.pdf");
        assert_eq!(results[0].metadata["domains"], json!(["hr"]));
        assert_eq!(results[3].metadata["domains"], json!(["engineering"]));
        for result in &results {
            assert!(result.metadata.get("updated_at").is_some());
        }

        // Entities attached to the linked chunk only.
        assert_eq!(results[0].entities.len(), 1);
        assert_eq!(results[0].entities[0].name, "Alice");
        assert!(
            results
                .iter()
                .skip(1)
                .all(|result| result.entities.is_empty()),
            "only the linked chunk carries entities"
        );
    });
}

// Domain filter on BOTH legs (lexical SQL-side, semantic application-side):
// the fused pool never leaves the requested domain; case-insensitive.
#[test]
fn hybrid_search_domain_filter() {
    let db = in_memory_db();
    let corpus = seed_corpus(&db);
    let index = MemoryIndex::new(MockEmbedding.vector_dim());
    index_corpus(&index, &corpus);

    with_searcher(&db, base_config(), &index, |searcher| {
        let searcher: &dyn Searcher = searcher;

        let hr = searcher.hybrid_search("policy", 10, Some("hr")).unwrap();
        assert_eq!(
            ids(&hr),
            vec![corpus.hr[0], corpus.conduct, corpus.hr[1]],
            "lexical [HR_BENEFITS, POL_CONDUCT] + semantic [HR_ONBOARDING] fused"
        );
        assert!(
            hr.iter()
                .all(|result| result.document_path != "/docs/engineering-design.pdf"),
            "no out-of-domain document in the pool"
        );

        let eng = searcher
            .hybrid_search("policy", 10, Some("engineering"))
            .unwrap();
        assert_eq!(
            ids(&eng),
            vec![corpus.engineering[0], corpus.engineering[1]],
            "no engineering chunk matches lexically: semantic leg only"
        );
        assert!(
            eng.iter()
                .all(|result| result.source_type == "semantic+pdf"),
        );

        // Case-insensitive.
        let upper = searcher.hybrid_search("policy", 10, Some("HR")).unwrap();
        assert_eq!(ids(&upper), ids(&hr));
    });
}

// ── reranker ──────────────────────────────────────────────────────────────

// The official boost (×1.5) composes with the freshness boost (×1.2) and
// flips the order: the official chunk leads despite the WORSE raw distance.
#[test]
fn reranker_official_boost_reorders_results() {
    let db = in_memory_db();
    let plain = seed_doc(&db, "markdown", "/docs/plain.md", None);
    let official = seed_doc(
        &db,
        "policy",
        "/docs/official.md",
        Some(r#"{"is_official":true}"#),
    );
    let closer = seed_chunk(&db, plain, "onboarding alpha", 0);
    let official_chunk = seed_chunk(&db, official, "benefits migration beta", 1);

    // Query embeds to the zero vector; distances: closer 1.0, official √2.
    // 1/√2 × 1.5 × 1.2 ≈ 1.273 > 1.0 × 1.2 = 1.2: the official chunk wins
    // despite the distance gap.
    let index = MemoryIndex::new(MockEmbedding.vector_dim());
    index
        .insert(closer as u32, &embed("onboarding alpha"))
        .unwrap();
    index
        .insert(official_chunk as u32, &embed("benefits migration beta"))
        .unwrap();

    let mut config = base_config();
    config.enable_lexical = false;

    with_searcher(&db, config, &index, |searcher| {
        let results = searcher.semantic_search("query", 10, None).unwrap();

        // 1/1.0·1.2 = 1.2 vs 1/√2·1.5·1.2 ≈ 1.273: the official chunk wins.
        assert_eq!(ids(&results), vec![official_chunk, closer]);
        assert_eq!(ranks(&results), vec![1, 2]);
        assert!(
            results[0].score > results[1].score,
            "the official boost must outweigh the distance gap"
        );

        assert_eq!(results[0].source_type, "semantic+policy");
        assert_eq!(results[0].metadata["is_official"], json!(true));
        assert_eq!(results[0].document_path, "/docs/official.md");
        assert_eq!(results[1].document_path, "/docs/plain.md");
        assert!(results[1].metadata.get("is_official").is_none());
    });
}

// ── empty queries ─────────────────────────────────────────────────────────

// Empty and whitespace-only queries return Ok(empty) on every entry point
// without touching a leg (oracle `nil, nil`).
#[test]
fn empty_queries_return_empty() {
    let db = in_memory_db();
    let corpus = seed_corpus(&db);
    let index = MemoryIndex::new(MockEmbedding.vector_dim());
    index_corpus(&index, &corpus);

    with_searcher(&db, base_config(), &index, |searcher| {
        let searcher: &dyn Searcher = searcher;
        for query in ["", "   \t "] {
            assert!(searcher.hybrid_search(query, 10, None).unwrap().is_empty());
            assert!(searcher.lexical_search(query, 10, None).unwrap().is_empty());
            assert!(
                searcher
                    .semantic_search(query, 10, Some("hr"))
                    .unwrap()
                    .is_empty()
            );
        }
    });
}
