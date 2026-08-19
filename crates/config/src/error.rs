//! Error type shared by every config loader in this crate.
//!
//! `ConfigError` is a [`thiserror`] enum covering I/O failures while reading a
//! config file, YAML/XML parse failures (each carrying the offending path and
//! the underlying parser error as `source`), and semantic validation failures
//! raised by [`crate::preset::Config::validate`]. (A regex-compile variant joins
//! this list with task 3.1b.)

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
    /// embeddings section missing required fields for its mode). Messages are
    /// kept close to the oracle's wording so parity diffs stay readable.
    #[error("invalid configuration: {message}")]
    Validation {
        /// Human-readable description of the violated invariant.
        message: String,
    },

    /// The `global.xml` ontology document could not be parsed or deserialized
    /// into [`crate::ontology::GlobalConfig`] (malformed XML, unexpected shape).
    #[error("parse global config {path}: {source}")]
    Xml {
        /// Path of the offending `global.xml`.
        path: String,
        /// Underlying parser / type-mismatch error from quick-xml.
        #[source]
        source: quick_xml::DeError,
    },
    // The onnx.yaml loader reuses `Io` / `Yaml`: design D4's frozen variant list has no
    // ONNX-specific variant, and both failure modes of that file map onto these two with
    // the path carried exactly like the main config.
}
