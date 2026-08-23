//! Crate error type.
//!
//! [`GraphError`] covers the failure classes of the graph crate: CEL
//! expression parse/evaluation failures (cel crate) and storage failures
//! from the `db` crate (SQLite is the source of truth, design D1).
//! Further variants are added as the module tasks land (1.2–1.9).
//!
//! Library error per workspace convention: `thiserror` with one variant per
//! failure class. Conversion from the upstream crates is provided via
//! `From`, so crate code can use `?` freely.

use thiserror::Error;

/// All error conditions surfaced by this crate.
#[derive(Debug, Error)]
pub enum GraphError {
    /// A CEL expression could not be parsed (syntax error).
    #[error("CEL parse error: {source}")]
    CelParse {
        /// The underlying cel parse error.
        #[source]
        source: cel::ParseErrors,
    },
    /// A CEL expression parsed but failed to evaluate (undeclared reference,
    /// type mismatch, function error, ...).
    #[error("CEL evaluation error: {source}")]
    CelEval {
        /// The underlying cel execution error.
        #[source]
        source: cel::ExecutionError,
    },
    /// A storage failure from the `db` crate: every read of entities, links,
    /// facts or chunks goes through the DAOs (design D1).
    #[error("storage error: {0}")]
    Db(#[from] db::DbError),
    /// A finder input was empty after normalization (a caller bug: an empty
    /// name or query cannot match — the oracle's "pattern must not be empty"
    /// for `FindEntityPartial`, task 1.3).
    #[error("empty query: {what} must not be empty")]
    EmptyQuery {
        /// Which argument was empty.
        what: &'static str,
    },
    /// The requested entity is not present in the index (the oracle's
    /// "start node %d not found" from `BFS`, task 1.4).
    #[error("entity {entity_id} not found in graph")]
    EntityNotFound {
        /// The missing entity row id.
        entity_id: i64,
    },
}

impl From<cel::ParseErrors> for GraphError {
    fn from(source: cel::ParseErrors) -> Self {
        GraphError::CelParse { source }
    }
}

impl From<cel::ExecutionError> for GraphError {
    fn from(source: cel::ExecutionError) -> Self {
        GraphError::CelEval { source }
    }
}
