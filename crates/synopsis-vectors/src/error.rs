//! Crate error type.
//!
//! [`VectorsError`] covers the whole vectors crate: argument and dimension
//! validation, a missing index, the ANN engine, filesystem I/O, the
//! SYNX fixture format (magic, version, truncation, zero dimensionality),
//! and the sidecar key manifest format (magic, truncation, trailing bytes).

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
    /// A requested on-disk resource does not exist: an index directory with
    /// no index (opening a path that was never created), or a sidecar
    /// `.keys` key manifest (usearch-wal-persistence task 3.2). The payload
    /// is the path that was looked up.
    #[error("index not found at {0}")]
    NotFound(String),
    /// The ANN engine reported a failure.
    #[error("engine error: {0}")]
    Engine(String),
    /// The `"lance"` engine was removed (post-migration-lance-removal,
    /// design D2): a config still selecting it gets a loud, actionable error
    /// instead of a silent engine swap.
    #[error("the \"lance\" engine was removed; the only engine is \"usearch\"")]
    EngineRemoved,
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
    /// A sidecar key manifest (`.keys`, usearch-wal-persistence task 3.2)
    /// does not start with the `"SKEY"` magic bytes.
    #[error("key manifest: bad magic")]
    KeysBadMagic,
    /// A sidecar key manifest is truncated: the header or a key record ends
    /// before the declared number of bytes. The payload says where.
    #[error("key manifest: truncated file ({0})")]
    KeysTruncated(String),
    /// A sidecar key manifest carries trailing bytes beyond the declared
    /// key count. The payload is the number of extra bytes and the declared
    /// count.
    #[error("key manifest: {0} trailing bytes after {1} declared keys")]
    KeysTrailingBytes(usize, u32),
}
