//! Error type shared by every parser, chunker and registry lookup in this crate.
//!
//! `IngestionError` is a [`thiserror`] enum (workspace convention, cf.
//! `config::ConfigError`). Parsers never fail hard: per-file failures are
//! collected in [`ParseResult::errors`](crate::types::ParseResult::errors) and
//! the walk continues (oracle contract, design D1). Chunkers and the source
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
    /// surfaced explicitly (oracle contract: `Register` errors on duplicates).
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
    /// routing table (task 1.9). The oracle fails loud on an unknown routing
    /// key instead of guessing a chunker.
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
    /// (ingestion-ner task 2.5; the oracle errors with "no valid domain
    /// configs").
    #[error("llm ner: at least one domain config is required")]
    LlmNerNoDomains,
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
