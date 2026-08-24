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
