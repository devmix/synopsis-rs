//! ANN index contract (trait) and LanceDB engine; replaces Go vec0 brute-force search.
//! No Go counterpart - new crate per design.md D1 (tier 0: no internal dependencies;
//! engine chosen by native-seam-spikes, ADR 0003).
//!
//! The public seam is [`VectorIndex`] (object-safe; consumers hold
//! `Arc<dyn VectorIndex>`), [`VectorIndexConfig`] (index/query parameters, defaults per
//! ADR 0003) and [`VectorsError`]. The LanceDB-backed engine ([`engine::LanceEngine`])
//! runs the async LanceDB API on a dedicated tokio runtime (design D2) behind sync
//! methods: call it only from sync contexts or `spawn_blocking` workers.
//!
//! Inputs are ready-made `(chunk_id, Vec<f32>)` pairs; `chunk_id` is the application-level
//! primary key (the SQLite chunk row id). The dimensionality is a configuration parameter -
//! this crate never calls the embedding crate, so the query path does not load the model.
//!
//! Module [`synx`] implements the SYNX binary fixture format (`vectors.bin`), the
//! oracle ↔ harness vector-dump contract (native-seam-spikes design D4): a streaming
//! reader and a chunk_id-sorted writer.

pub mod engine;
pub mod synx;

pub use engine::LanceEngine;

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

/// Index and query parameters for the ANN engine.
///
/// Defaults are the ADR 0003 configuration (IvfHnswSq, u8 scalar quantization, L2):
/// `dim = 1024` (bge-m3), `m = 16`, `ef_construction = 100`, `num_partitions = 256`,
/// `nprobes = 32`, `ef_search = 200`. `nprobes` and `ef_search` are runtime-tunable
/// (ADR 0003 mitigation #1): raising them trades latency for recall without rebuilding
/// the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorIndexConfig {
    /// Vector dimensionality (bge-m3: 1024).
    pub dim: usize,
    /// HNSW graph degree M.
    pub m: usize,
    /// HNSW efConstruction: candidate list size during index build.
    pub ef_construction: usize,
    /// Number of IVF partitions (coarse-quantizer centroids).
    pub num_partitions: usize,
    /// IVF partitions probed per query.
    pub nprobes: usize,
    /// HNSW efSearch: candidate list size during query.
    pub ef_search: usize,
}

impl VectorIndexConfig {
    /// Creates a configuration, validating all field invariants.
    pub fn new(
        dim: usize,
        m: usize,
        ef_construction: usize,
        num_partitions: usize,
        nprobes: usize,
        ef_search: usize,
    ) -> Result<Self, VectorsError> {
        let config = Self {
            dim,
            m,
            ef_construction,
            num_partitions,
            nprobes,
            ef_search,
        };
        config.validate()?;
        Ok(config)
    }

    /// Validates field invariants:
    ///
    /// - `dim > 0`
    /// - `m > 0`
    /// - `ef_construction >= m` (HNSW invariant)
    /// - `num_partitions > 0`
    /// - `nprobes > 0`
    /// - `ef_search > 0`
    pub fn validate(&self) -> Result<(), VectorsError> {
        if self.dim == 0 {
            return Err(VectorsError::InvalidArgument("dim must be > 0".to_string()));
        }
        if self.m == 0 {
            return Err(VectorsError::InvalidArgument("m must be > 0".to_string()));
        }
        if self.ef_construction < self.m {
            return Err(VectorsError::InvalidArgument(format!(
                "ef_construction ({}) must be >= m ({})",
                self.ef_construction, self.m
            )));
        }
        if self.num_partitions == 0 {
            return Err(VectorsError::InvalidArgument(
                "num_partitions must be > 0".to_string(),
            ));
        }
        if self.nprobes == 0 {
            return Err(VectorsError::InvalidArgument(
                "nprobes must be > 0".to_string(),
            ));
        }
        if self.ef_search == 0 {
            return Err(VectorsError::InvalidArgument(
                "ef_search must be > 0".to_string(),
            ));
        }
        Ok(())
    }

    /// Validates a top-k search request: `k > 0` and the query length equals `dim`.
    ///
    /// The engine calls this before every search (task 1.3).
    pub fn validate_search(&self, query: &[f32], k: usize) -> Result<(), VectorsError> {
        if k == 0 {
            return Err(VectorsError::InvalidArgument(
                "top-k must be > 0".to_string(),
            ));
        }
        if query.len() != self.dim {
            return Err(VectorsError::DimensionMismatch {
                expected: self.dim,
                actual: query.len(),
            });
        }
        Ok(())
    }
}

impl Default for VectorIndexConfig {
    /// ADR 0003 configuration: 1024-dim, M=16, efConstruction=100, 256 partitions,
    /// nprobes=32, efSearch=200.
    fn default() -> Self {
        Self {
            dim: 1024,
            m: 16,
            ef_construction: 100,
            num_partitions: 256,
            nprobes: 32,
            ef_search: 200,
        }
    }
}

/// Disk-backed approximate nearest-neighbour index keyed by chunk id.
///
/// The trait is object-safe (`Send + Sync`, no generics or `async fn`): consumers hold
/// `Arc<dyn VectorIndex>`. All methods are sync - the LanceDB engine runs its async API
/// on a dedicated tokio runtime (design D2), so implementations must be called only from
/// sync contexts or `spawn_blocking` workers, never from inside an async task.
///
/// Semantics:
///
/// - `chunk_id` is the application-level primary key (SQLite chunk row id); uniqueness is
///   the caller's responsibility - replace via `delete_by_chunk_ids` + `insert` or
///   `rebuild`.
/// - Cascade protocol (design D3): the caller invokes `delete_by_chunk_ids` BEFORE
///   deleting chunk rows in SQLite; search may surface orphaned vectors and the consumer
///   filters them against SQLite. `chunk_ids`/`count` are the reconciliation primitives.
pub trait VectorIndex: Send + Sync {
    /// Stores `vector` under `chunk_id`. The vector length must equal the configured dim.
    fn insert(&self, chunk_id: u32, vector: &[f32]) -> Result<(), VectorsError>;

    /// Stores many vectors in one call; the engine batches them into Arrow batches
    /// (~1000 rows per design D4).
    fn insert_batch(&self, rows: &[(u32, &[f32])]) -> Result<(), VectorsError>;

    /// Top-k nearest neighbours of `query`, as `(chunk_id, distance)` pairs sorted by L2
    /// distance ascending. An empty index returns an empty vec, not an error. Requires
    /// `k > 0` and `query.len() == dim` (see [`VectorIndexConfig::validate_search`]).
    fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u32, f32)>, VectorsError>;

    /// Removes vectors by chunk id. Idempotent for ids that are not present.
    fn delete_by_chunk_ids(&self, chunk_ids: &[u32]) -> Result<(), VectorsError>;

    /// All chunk ids currently stored, in no particular order.
    fn chunk_ids(&self) -> Result<Vec<u32>, VectorsError>;

    /// Number of vectors currently stored.
    fn count(&self) -> Result<u64, VectorsError>;

    /// (Re)builds the ANN index over the stored vectors with the configured parameters
    /// (IvfHnswSq per ADR 0003).
    fn build_index(&self) -> Result<(), VectorsError>;

    /// Atomically replaces the entire content with `rows` (drop + recreate).
    fn rebuild(&self, rows: &[(u32, Vec<f32>)]) -> Result<(), VectorsError>;
}

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (fixtures always parse).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn config_with(mutate: impl FnOnce(&mut VectorIndexConfig)) -> VectorIndexConfig {
        let mut config = VectorIndexConfig::default();
        mutate(&mut config);
        config
    }

    #[test]
    fn defaults_match_adr_0003() {
        let config = VectorIndexConfig::default();
        assert_eq!(config.dim, 1024);
        assert_eq!(config.m, 16);
        assert_eq!(config.ef_construction, 100);
        assert_eq!(config.num_partitions, 256);
        assert_eq!(config.nprobes, 32);
        assert_eq!(config.ef_search, 200);
        config.validate().expect("defaults must be valid");
    }

    #[test]
    fn new_accepts_valid_config() {
        let config = VectorIndexConfig::new(512, 8, 64, 64, 16, 100).unwrap();
        assert_eq!(config.dim, 512);
        assert_eq!(config.ef_construction, 64);
    }

    #[test]
    fn validate_rejects_invalid_fields() {
        assert!(config_with(|c| c.dim = 0).validate().is_err());
        assert!(config_with(|c| c.m = 0).validate().is_err());
        // ef_construction < m violates the HNSW invariant.
        assert!(config_with(|c| c.ef_construction = 10).validate().is_err());
        assert!(config_with(|c| c.num_partitions = 0).validate().is_err());
        assert!(config_with(|c| c.nprobes = 0).validate().is_err());
        assert!(config_with(|c| c.ef_search = 0).validate().is_err());
    }

    #[test]
    fn new_propagates_validation_errors() {
        assert!(VectorIndexConfig::new(0, 16, 100, 256, 32, 200).is_err());
        assert!(VectorIndexConfig::new(1024, 16, 10, 256, 32, 200).is_err());
        assert!(VectorIndexConfig::new(1024, 0, 100, 256, 32, 200).is_err());
        assert!(VectorIndexConfig::new(1024, 16, 100, 0, 32, 200).is_err());
        assert!(VectorIndexConfig::new(1024, 16, 100, 256, 0, 200).is_err());
        assert!(VectorIndexConfig::new(1024, 16, 100, 256, 32, 0).is_err());
    }

    #[test]
    fn validate_search_rejects_zero_k_and_bad_dim() {
        let config = VectorIndexConfig::default();
        let query = vec![0.0f32; 1024];
        assert!(config.validate_search(&query, 0).is_err());
        let short = vec![0.0f32; 512];
        match config.validate_search(&short, 10) {
            Err(VectorsError::DimensionMismatch { expected, actual }) => {
                assert_eq!((expected, actual), (1024, 512));
            }
            other => panic!("expected DimensionMismatch, got {other:?}"),
        }
        config
            .validate_search(&query, 10)
            .expect("valid search params");
    }
}
