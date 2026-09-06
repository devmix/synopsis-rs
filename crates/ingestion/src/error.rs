//! Error type shared by every parser, chunker and registry lookup in this crate.
//!
//! `IngestionError` is a [`thiserror`] enum (workspace convention, cf.
//! `config::ConfigError`). Parsers never fail hard: per-file failures are
//! collected in [`ParseResult::errors`](crate::types::ParseResult::errors) and
//! the walk continues (design D1). Chunkers and the source
//! registry (task 1.6) return the error directly.

use std::path::PathBuf;

use thiserror::Error;

/// Error returned by ingestion parsers, chunkers and the source registry.
#[derive(Debug, Error)]
pub enum IngestionError {
    /// A source file could not be read or the source tree could not be walked
    /// (missing path, permissions, …). Parsers collect this in
    /// [`ParseResult::errors`](crate::types::ParseResult::errors); the walk
    /// continues.
    #[error("read source file {path}: {source}")]
    Io {
        /// Path that failed to open.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// A JSON document (json / mediawiki source) could not be parsed —
    /// malformed syntax or wrong shape. Non-fatal: collected in
    /// [`ParseResult::errors`](crate::types::ParseResult::errors).
    #[error("parse JSON {path}: {source}")]
    Json {
        /// Path of the offending JSON file.
        path: PathBuf,
        /// Underlying deserializer error from serde_json.
        #[source]
        source: serde_json::Error,
    },

    /// HTML-to-markdown conversion failed (webpage source, task 1.8). The
    /// converter's own error type is deliberately not carried: the html crate
    /// is chosen in task 1.8 and its error is mapped to a message there.
    #[error("convert HTML {path}: {message}")]
    HtmlConversion {
        /// Path of the offending HTML file.
        path: PathBuf,
        /// Human-readable description of the conversion failure.
        message: String,
    },

    /// A source type name from `global.xml` has no registered implementation.
    /// Design D5: an unknown type is an explicit error, never a silent skip.
    #[error("unknown source type {0:?}")]
    UnknownSourceType(String),

    /// A source type was registered twice (registry, task 1.6). Registration
    /// happens once at pipeline start, so a duplicate is a programmer error —
    /// surfaced explicitly (registration errors on duplicates).
    #[error("source type {0:?} already registered")]
    AlreadyRegistered(String),

    /// A file extension matched no registered parser (design D5).
    #[error("unsupported file extension {0:?}")]
    UnsupportedExtension(String),

    /// A chunking strategy is not recognized (chunkers, task 1.3+; the config
    /// crate keeps unknown strategy words in `ChunkingStrategy::Unknown`).
    #[error("unknown chunking strategy {0:?}")]
    UnknownStrategy(String),

    /// A document's `source_type` has no chunker in the unstructured source's
    /// routing table (task 1.9). An unknown routing key is an explicit error
    /// instead of guessing a chunker.
    #[error("unknown source type {0:?} for unstructured chunk routing")]
    ChunkRouting(String),

    /// A prompt template override file could not be read (NER prompts,
    /// ingestion-ner task 2.2).
    #[error("read prompt template {path}: {source}")]
    PromptTemplateIo {
        /// Path of the override file.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// A prompt template (embedded default or user override) failed to parse
    /// (NER prompts, ingestion-ner task 2.2).
    #[error("parse prompt template {name}: {source}")]
    PromptTemplateParse {
        /// Short template name (`"system"` / `"user"`).
        name: String,
        /// Underlying minijinja parse error.
        source: minijinja::Error,
    },

    /// Rendering a prompt template failed (NER prompts, ingestion-ner
    /// task 2.2).
    #[error("render prompt template {name}: {source}")]
    PromptTemplateRender {
        /// Short template name (`"system"` / `"user"`).
        name: String,
        /// Underlying minijinja render error.
        source: minijinja::Error,
    },

    /// A SQLite storage failure (LLM-NER cache, ingestion-ner task 2.4): the
    /// db crate is the source of truth for the lazily-created
    /// `llm_ner_cache` table (design D6). A broken database must not look
    /// empty — the failure propagates instead of degrading to a cache miss
    /// (db convention, cf. `db::AppKv`).
    #[error("storage error: {0}")]
    Db(#[from] db::DbError),

    /// Serializing an LLM-NER cache entry to JSON failed (ingestion-ner
    /// task 2.4). In practice unreachable for validated results (finite
    /// confidence), but the API is total.
    #[error("llm ner cache: serialize entry: {source}")]
    NerCacheJson {
        /// Underlying serde_json error.
        #[source]
        source: serde_json::Error,
    },

    /// An LLM call failed during NER extraction (ingestion-ner task 2.5,
    /// design D5/D10): configuration, HTTP status, transport, or retry
    /// exhaustion. The llm crate's error is the source of truth; the
    /// failure is fatal for the extraction call (no partial results).
    #[error("llm ner call: {0}")]
    Llm(#[from] llm::LlmError),

    /// The model's NER response content is not the expected
    /// `{entities, relations}` JSON (ingestion-ner task 2.5). The pure
    /// parser ([`crate::ner::parse_llm_response`]) returns the
    /// serde_json::Error; this provider boundary names the domain and maps
    /// it (design D10: LLM failures are fatal for the call).
    #[error("parse llm ner response for domain {domain}: {source}")]
    LlmNerParse {
        /// Domain the failed response was rendered for.
        domain: String,
        /// Underlying serde_json error from the response parse.
        #[source]
        source: serde_json::Error,
    },

    /// The LLM provider was constructed without any domain config
    /// (ingestion-ner task 2.5).
    #[error("llm ner: at least one domain config is required")]
    LlmNerNoDomains,

    /// A configured NER stage `"prose"` has no implementation (ingestion-ner
    /// task 2.6): prose NER is deferred by human decision 2026-08-23 — the
    /// Go-only statistical provider has no Rust equivalent and a second
    /// ONNX stack was rejected. The config parser still accepts the `"prose"`
    /// word (the strict `NerMethod` enum keeps it); the failure surfaces at
    /// provider construction instead.
    #[error(
        "ner stage \"prose\" is not implemented: prose NER is deferred \
        (human decision 2026-08-23 — Go-only statistical provider, no Rust \
        equivalent, second ONNX stack rejected); configure \"regex\" and/or \
        \"llm\" instead"
    )]
    ProseNerDeferred,

    /// The document id passed to the entity resolver is not a valid
    /// database row id (task 2.8): the resolver refuses to link provenance
    /// to a nonexistent row instead of letting the FK constraint fail
    /// mid-batch.
    #[error("invalid document id {0}: must be a positive row id")]
    InvalidDocumentId(i64),

    /// An entity candidate found in the resolver's blocking index no longer
    /// exists in the database and a full rehydration did not recover it
    /// (task 2.8, design D9): the index and the database diverged beyond
    /// the single-retry recovery path.
    #[error(
        "entity candidate {0} is gone and could not be re-resolved after \
        index rehydration"
    )]
    EntityCandidateGone(i64),

    /// Serializing an entity's scoped metadata to JSON failed (task 2.8).
    /// In practice unreachable (the input is already a `serde_json::Map`),
    /// but the API is total.
    #[error("entity metadata: serialize scoped JSON: {source}")]
    EntityMetadataJson {
        /// Underlying serde_json error.
        #[source]
        source: serde_json::Error,
    },

    /// The ingest root exists but is not a directory (pipeline task 3.4):
    /// the ingest rejects a file root before doing anything.
    #[error("source path {path} is not a directory")]
    NotADirectory {
        /// The offending path.
        path: PathBuf,
    },

    /// Parsing produced no documents but did produce errors (pipeline
    /// task 3.4): the run fails instead of reporting a successful no-op.
    #[error("no documents parsed, {count} errors occurred")]
    NoDocumentsParsed {
        /// Number of parse errors collected.
        count: usize,
    },

    /// Embedding generation failed (pipeline task 3.4): the embedding
    /// provider is the source of truth.
    #[error("embedding: {0}")]
    Embedding(#[from] embedding::EmbeddingError),

    /// The embedding provider returned a different vector count than the
    /// requested text count (pipeline task 3.4): continuing would silently
    /// misalign every vector from that batch on, so the document fails
    /// instead of storing misaligned vectors.
    #[error(
        "embedding count mismatch in batch {batch} of {total_batches}: \
         expected {expected} vectors, got {actual}"
    )]
    EmbeddingCountMismatch {
        /// 1-based batch number that failed.
        batch: usize,
        /// Total number of batches for the document.
        total_batches: usize,
        /// Number of texts in the batch.
        expected: usize,
        /// Number of vectors the provider returned.
        actual: usize,
    },

    /// A vector-index write failed (pipeline task 3.4, design D5): vectors
    /// are written after the SQLite commit, so a failure here leaves chunks
    /// without vectors; orphan reconciliation repairs the divergence (the
    /// chunk row is the source of truth).
    #[error("vector index: {0}")]
    Vectors(#[from] vectors::VectorsError),

    /// Serializing document metadata to JSON failed (pipeline task 3.4).
    /// In practice unreachable (parser-produced values are finite), but the
    /// API is total (cf. [`NerCacheJson`](Self::NerCacheJson)).
    #[error("document metadata: serialize JSON: {source}")]
    MetadataJson {
        /// Underlying serde_json error.
        #[source]
        source: serde_json::Error,
    },

    /// No configured source root contains the given path (pipeline task 3.7).
    #[error("no configured source contains {path}")]
    NoSourceForPath {
        /// The path that matched no configured source root.
        path: String,
    },

    /// A cross-domain entity-linking failure (pipeline task 3.8): the graph
    /// crate is the source of truth for CEL/linker failures. The linker's
    /// per-link failures are recorded in its result and never fatal (design
    /// D8).
    #[error("entity linking: {0}")]
    Graph(#[from] graph::GraphError),

    /// A queue-task DAO failure (event-queue-incremental-linking task 1.2):
    /// the db crate is the source of truth for the `queue_tasks` state
    /// machine. Producers and the worker call [`db::QueueTaskDao`] methods
    /// that return [`db::QueueTaskError`].
    #[error("queue task: {0}")]
    QueueTask(#[from] db::QueueTaskError),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::error::Error as _;

    use super::*;

    #[test]
    fn error_messages_carry_context() {
        let io_err = IngestionError::Io {
            path: PathBuf::from("/data/missing.md"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "gone"),
        };
        let msg = io_err.to_string();
        assert!(msg.contains("/data/missing.md"), "{msg}");
        assert!(msg.contains("gone"), "{msg}");

        let unknown = IngestionError::UnknownSourceType("xml".to_owned());
        assert_eq!(unknown.to_string(), r#"unknown source type "xml""#);

        let strategy = IngestionError::UnknownStrategy("rolling".to_owned());
        assert_eq!(
            strategy.to_string(),
            r#"unknown chunking strategy "rolling""#
        );

        let routing = IngestionError::ChunkRouting("mediawiki".to_owned());
        assert_eq!(
            routing.to_string(),
            r#"unknown source type "mediawiki" for unstructured chunk routing"#
        );

        let invalid_id = IngestionError::InvalidDocumentId(0);
        assert_eq!(
            invalid_id.to_string(),
            "invalid document id 0: must be a positive row id"
        );

        let gone = IngestionError::EntityCandidateGone(7);
        assert_eq!(
            gone.to_string(),
            "entity candidate 7 is gone and could not be re-resolved after index rehydration"
        );

        let metadata_json = IngestionError::EntityMetadataJson {
            source: serde_json::from_str::<serde_json::Value>("not json").unwrap_err(),
        };
        assert!(
            metadata_json
                .to_string()
                .starts_with("entity metadata: serialize scoped JSON:"),
            "{}",
            metadata_json
        );

        let prose = IngestionError::ProseNerDeferred;
        assert_eq!(
            prose.to_string(),
            "ner stage \"prose\" is not implemented: prose NER is deferred \
             (human decision 2026-08-23 — Go-only statistical provider, no Rust \
             equivalent, second ONNX stack rejected); configure \"regex\" and/or \
             \"llm\" instead"
        );
    }

    #[test]
    fn io_variant_keeps_the_source_error() {
        let source = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "locked");
        let io_err = IngestionError::Io {
            path: PathBuf::from("/data/x.md"),
            source,
        };
        assert!(io_err.source().is_some());
    }
}
