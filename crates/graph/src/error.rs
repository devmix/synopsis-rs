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
    /// name or query cannot match — the partial entity finder rejects an
    /// empty pattern, task 1.3).
    #[error("empty query: {what} must not be empty")]
    EmptyQuery {
        /// Which argument was empty.
        what: &'static str,
    },
    /// The requested entity is not present in the index (the BFS start-node
    /// check, task 1.4).
    #[error("entity {entity_id} not found in graph")]
    EntityNotFound {
        /// The missing entity row id.
        entity_id: i64,
    },
    /// An ontology linking rule evaluated to a non-boolean value (task 1.9):
    /// rules are expected to evaluate to a boolean, but the `cel` crate's
    /// `Program::compile` is parse-only, so the type check happens at
    /// evaluation.
    #[error("linking rule {name:?} must evaluate to a boolean")]
    NonBooleanRule {
        /// The rule's name.
        name: String,
    },
    /// A cached LLM-linker decision could not be serialized to JSON (task
    /// 1.10): the `LlmLinkerCache::set` write path. Deserialization failures
    /// are treated as cache misses inside `LlmLinkerCache::get` instead.
    #[error("linker decision JSON: {source}")]
    DecisionJson {
        /// The underlying serde error.
        #[source]
        source: serde_json::Error,
    },
    /// A prompt-template override file exists but could not be read (llm change
    /// 2.1, design D3). A *missing* file is not an error — it falls back to the
    /// embedded default.
    #[error("prompt template: read {path}: {source}")]
    PromptTemplateIo {
        /// The override file path that failed to read.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A prompt template (embedded default or user override) failed to parse
    /// (llm change 2.1, design D3). Detected at load time, not first render.
    #[error("prompt template {name}: parse: {source}")]
    PromptTemplateParse {
        /// Which template: `system` or `user`.
        name: String,
        /// The underlying minijinja parse error.
        #[source]
        source: minijinja::Error,
    },
    /// A prompt template failed to render (llm change 2.1, design D3): a
    /// registered helper was misused, or an override template references a
    /// missing field under a strict undefined behavior.
    #[error("prompt template {name}: render: {source}")]
    PromptTemplateRender {
        /// Which template: `system` or `user`.
        name: String,
        /// The underlying minijinja render error.
        #[source]
        source: minijinja::Error,
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
