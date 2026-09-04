//! Error type shared by every config loader in this crate.
//!
//! `ConfigError` is a [`thiserror`] enum covering I/O failures while reading a
//! config file, YAML/XML parse failures (each carrying the offending path and
//! the underlying parser error as `source`), semantic validation failures
//! raised by config loaders, and regex-compile failures for extraction rules
//! (design D5: patterns are compiled at load time).

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
    /// embeddings section missing required fields for its mode). Messages
    /// use the established wording.
    #[error("invalid configuration: {message}")]
    Validation {
        /// Human-readable description of the violated invariant.
        message: String,
    },

    /// An ontology XML document (`global.xml` or a `domains/*.xml` file) could not be parsed or
    /// deserialized into its typed shape (malformed XML, unexpected structure).
    #[error("parse ontology XML {path}: {source}")]
    Xml {
        /// Path of the offending XML file.
        path: String,
        /// Underlying parser / type-mismatch error from quick-xml.
        #[source]
        source: quick_xml::DeError,
    },

    /// A `<regex>` extraction rule's pattern failed to compile at load time (design D5).
    /// An invalid pattern is a typed error instead of a process crash.
    #[error("invalid regex pattern for rule {rule:?} in {file}: {source}")]
    Regex {
        /// Path of the ontology file containing the offending rule.
        file: String,
        /// `id` attribute of the `<regex>` element (always non-empty — an empty id fails
        /// validation before compilation).
        rule: String,
        /// Underlying pattern error from the `regex` crate.
        #[source]
        source: regex::Error,
    },
    // The onnx.yaml loader reuses `Io` / `Yaml`: design D4's frozen variant list has no
    // ONNX-specific variant, and both failure modes of that file map onto these two with
    // the path carried exactly like the main config.
}
