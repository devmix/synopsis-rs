//! Error type for the search pipeline (design D9).
//!
//! `SearchError` is a [`thiserror`] enum (workspace convention, cf.
//! `config::ConfigError`). A hybrid search where **both** sub-search legs
//! fail is a hard error carrying both causes; a single leg failing degrades
//! to the other leg (task 4.6). An empty query returns `Ok(empty vec)`
//! (oracle returns `nil, nil`), never an error.

use thiserror::Error;

/// Error returned by the search pipeline.
#[derive(Debug, Error)]
pub enum SearchError {
    /// Both sub-search legs failed during a hybrid search; carries both
    /// causes (design D9).
    #[error("both sub-searches failed — lexical: {lexical}; semantic: {semantic}")]
    BothSubSearchesFailed {
        /// Lexical (FTS5) leg failure.
        lexical: String,
        /// Semantic (vector) leg failure.
        semantic: String,
    },

    /// The lexical (FTS5) leg failed.
    #[error("lexical sub-search failed: {0}")]
    Lexical(String),

    /// The semantic (vector) leg failed.
    #[error("semantic sub-search failed: {0}")]
    Semantic(String),

    /// A database access failed (chunk/document/entity DAOs).
    #[error("database error: {0}")]
    Db(#[from] db::DbError),

    /// Query embedding generation failed.
    #[error("embedding error: {0}")]
    Embedding(#[from] embedding::EmbeddingError),

    /// The vector index query failed.
    #[error("vector index error: {0}")]
    Vector(#[from] vectors::VectorsError),
}
