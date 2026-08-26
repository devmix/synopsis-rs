//! Hybrid (full-text + vector) search with Reciprocal Rank Fusion.
//!
//! Oracle mapping: `../synopsis/internal/search` (design D1). Task 4.1
//! delivers the shared result types and the RRF fusion core
//! ([`reciprocal_rank_fusion`]); task 4.2 adds the lexical and semantic
//! sub-searchers ([`LexicalSearcher`] / [`SemanticSearcher`]); task 4.3
//! adds the enricher ([`Enricher`] — document metadata, merged source
//! type, RFC3339 `updated_at`, reranker flags, domains and chunk
//! entities); task 4.4 adds the reranker ([`Reranker`] — business rules,
//! freshness and authority boosts with re-rank); the graph expander and
//! the `Searcher` trait land in tasks 4.5–4.6.
//!
//! Result types (oracle `search.go` / `lexical_search.go` /
//! `semantic_search.go`):
//!
//! - [`LexicalHit`] / [`SemanticHit`] are raw sub-search hits before
//!   fusion. Both carry a `score` where **lower is better** (FTS5 `bm25()`
//!   and cosine distance respectively).
//! - [`SearchResult`] is a fused, ranked hit: the chunk row fields, the
//!   calibrated score (higher is better), the 1-based [`SearchResult::rank`],
//!   the [`SourceType`] wire word (merged with the document source type by
//!   the enricher), and the enrichment slots (`document_path`, `metadata`,
//!   `entities`) that tasks 4.3/4.5 fill after fusion.

pub mod enrich;
pub mod error;
pub mod lexical;
pub mod rerank;
pub mod rrf;
pub mod semantic;

pub use enrich::Enricher;
pub use error::SearchError;
pub use lexical::LexicalSearcher;
pub use rerank::Reranker;
pub use rrf::{DEFAULT_RRF_K, reciprocal_rank_fusion};
pub use semantic::SemanticSearcher;

/// Which sub-search produced a hit (or both, when the chunk is in both
/// ranked lists).
///
/// The pre-enrichment vocabulary: [`SearchResult::source_type`] carries
/// [`Self::as_str`] until the enricher merges in the document source type
/// (design D6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceType {
    /// Hit came only from the lexical (FTS5/BM25) list.
    Lexical,
    /// Hit came only from the semantic (vector) list.
    Semantic,
    /// Hit appears in both lists (highest RRF contribution).
    Hybrid,
}

impl SourceType {
    /// The wire word used in MCP tool responses and parity diffs (frozen
    /// contract: `"lexical" | "semantic" | "hybrid"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lexical => "lexical",
            Self::Semantic => "semantic",
            Self::Hybrid => "hybrid",
        }
    }
}

/// One raw FTS5 hit before fusion (oracle `LexicalSearchResult`).
#[derive(Debug, Clone, PartialEq)]
pub struct LexicalHit {
    /// Chunk row id.
    pub chunk_id: i64,
    /// The chunk text.
    pub chunk_text: String,
    /// Owning document id.
    pub document_id: i64,
    /// Position of the chunk within its document.
    pub sequence_num: i64,
    /// Start offset in the original text, if any.
    pub start_offset: Option<i64>,
    /// End offset in the original text, if any.
    pub end_offset: Option<i64>,
    /// Raw FTS5 `bm25()` score (lower is better in SQLite).
    pub score: f64,
}

/// One raw vector hit before fusion (oracle `SemanticSearchResult`).
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticHit {
    /// Chunk row id.
    pub chunk_id: i64,
    /// The chunk text.
    pub chunk_text: String,
    /// Owning document id.
    pub document_id: i64,
    /// Position of the chunk within its document.
    pub sequence_num: i64,
    /// Start offset in the original text, if any.
    pub start_offset: Option<i64>,
    /// End offset in the original text, if any.
    pub end_offset: Option<i64>,
    /// Distance to the query embedding (lower is better).
    pub score: f64,
}

/// One fused, ranked hit (oracle `SearchResult`).
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    /// Chunk row id.
    pub chunk_id: i64,
    /// The chunk text.
    pub chunk_text: String,
    /// Owning document id.
    pub document_id: i64,
    /// Position of the chunk within its document.
    pub sequence_num: i64,
    /// Start offset in the original text, if any.
    pub start_offset: Option<i64>,
    /// End offset in the original text, if any.
    pub end_offset: Option<i64>,
    /// Document path; empty until the enricher fills it (task 4.3).
    pub document_path: String,
    /// Final calibrated score, higher is better (0.7·rrf + 0.3·bm25, both
    /// min-max normalized).
    pub score: f64,
    /// 1-based rank, assigned after the fusion sort.
    pub rank: usize,
    /// The wire source word: [`SourceType::as_str`] before enrichment,
    /// merged with the document source type by the enricher
    /// (`"lexical" + "pdf" → "lexical+pdf"`, design D6).
    pub source_type: String,
    /// Enrichment bag (document metadata, reranker flags, graph context);
    /// empty until tasks 4.3–4.5 fill it.
    pub metadata: serde_json::Map<String, serde_json::Value>,
    /// Entities attached to the chunk; empty until the enricher fills it
    /// (task 4.3).
    pub entities: Vec<db::Entity>,
}

/// Normalize a domain filter for matching: trim, collapse whitespace,
/// lowercase ([`db::utils::normalize`]). A domain that is empty after
/// normalization means "no filter" (`None`).
///
/// Stored domains are canonically lowercase (ontology XML), so normalizing
/// the input makes domain filtering case-insensitive on both sub-search legs
/// without changing the db crate's SQL contract.
pub(crate) fn normalize_domain(domain: Option<&str>) -> Option<String> {
    domain
        .map(db::utils::normalize)
        .filter(|normalized| !normalized.is_empty())
}

/// Document domains from `metadata_json` `$.domain`: a string or an array of
/// strings, each normalized (whitespace-collapsed, lowercased); non-string
/// array members, empty values, and malformed or missing JSON yield no
/// domains. Shared by the semantic leg's application-side domain filter
/// (design D3) and the enricher's `domains` metadata key (design D6).
pub(crate) fn document_domains(metadata_json: Option<&str>) -> Vec<String> {
    let Some(json) = metadata_json else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    match value.get("domain") {
        Some(serde_json::Value::String(domain)) => {
            let normalized = db::utils::normalize(domain);
            if normalized.is_empty() {
                Vec::new()
            } else {
                vec![normalized]
            }
        }
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(db::utils::normalize)
            .filter(|domain| !domain.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_domain_normalizes_and_drops_empty() {
        assert_eq!(normalize_domain(None), None);
        assert_eq!(normalize_domain(Some("")), None);
        assert_eq!(normalize_domain(Some("   \t ")), None);
        assert_eq!(normalize_domain(Some("HR")), Some("hr".to_string()));
        assert_eq!(
            normalize_domain(Some("  Eng   Dept ")),
            Some("eng dept".to_string())
        );
    }
}
