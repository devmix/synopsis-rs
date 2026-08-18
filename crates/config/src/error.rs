//! Error type shared by every config loader in this crate.
//!
//! `ConfigError` is a [`thiserror`] enum covering I/O failures while reading a
//! config file, YAML/XML/regex parse or compile failures (each carrying the
//! offending path and the underlying parser error as `source`), and semantic
//! validation failures raised by [`crate::preset::Config::validate`].

use thiserror::Error;

/// Error returned when loading or validating any configuration source.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The config file could not be read from disk (missing path, permission, …).
    #[error("read config file {path}: {source}")]
    Io {
        /// Path that failed to open.
        path: String,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// The YAML document could not be parsed or deserialized into `Config`.
    #[error("parse config YAML {path}: {source}")]
    Yaml {
        /// Path of the offending YAML file.
        path: String,
        /// Underlying parser / type-mismatch error from the YAML library.
        #[source]
        source: noyalib::Error,
    },

    /// A structurally valid config failed semantic validation (e.g. an
    /// embeddings section missing required fields for its mode).
    #[error("invalid configuration: {message}")]
    Validation {
        /// Human-readable description of the violated invariant.
        message: String,
    },
    // The `Xml`, `Regex` and ONNX variants are added by tasks 3.x / 2.1 as the
    // ontology and onnx.yaml loaders land (design D4).
}
