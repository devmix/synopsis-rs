//! Crate error type.
//!
//! [`VectorsError`] covers the whole vectors crate: argument and dimension
//! validation, a missing index, the LanceDB engine, filesystem I/O, and the
//! SYNX fixture format (magic, version, truncation, zero dimensionality).

use thiserror::Error;

/// Errors produced by the vectors crate.
#[derive(Debug, Error)]
pub enum VectorsError {
    /// A configuration or call parameter violated a documented invariant.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    /// A vector length does not match the index dimensionality.
    #[error("dimension mismatch: expected {expected}, got {actual}")]
    DimensionMismatch {
        /// Dimensionality the index is configured with.
        expected: usize,
        /// Length of the offending vector.
        actual: usize,
    },
    /// The requested index does not exist (e.g. opening a path with no table).
    /// The payload is the data directory that was looked up.
    #[error("index not found at {0}")]
    NotFound(String),
    /// The ANN engine (LanceDB) reported a failure.
    #[error("engine error: {0}")]
    Engine(String),
    /// A failure while accessing the on-disk index.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A SYNX fixture file does not start with the `"SYNX"` magic bytes.
    #[error("SYNX: bad magic")]
    SyNxBadMagic,
    /// A SYNX fixture file declares an unsupported format version.
    #[error("SYNX: unsupported version {0} (expected 1)")]
    SyNxBadVersion(u32),
    /// A SYNX fixture file is truncated: the header or a row ends before its
    /// promised number of bytes. The payload says where.
    #[error("SYNX: truncated file ({0})")]
    SyNxTruncated(String),
    /// A SYNX fixture file declares zero dimensionality.
    #[error("SYNX: dim must be > 0")]
    SyNxZeroDim,
}
