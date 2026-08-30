//! ANN index contract (trait) and the LanceDB / USearch engines; replaces Go vec0
//! brute-force search. No Go counterpart - new crate per design.md D1 (tier 0: no
//! internal dependencies; engine chosen by native-seam-spikes, ADR 0003).
//!
//! The public seam is [`VectorIndex`] (object-safe; consumers hold
//! `Arc<dyn VectorIndex>`), [`VectorIndexConfig`] (index/query parameters, defaults per
//! ADR 0003) and [`VectorsError`]. Two engines implement the trait:
//!
//! - [`engine::LanceEngine`] (feature `engine-lance`) runs the async LanceDB API on a
//!   dedicated tokio runtime (design D2) behind sync methods: call it only from sync
//!   contexts or `spawn_blocking` workers.
//! - [`usearch_engine::UsearchEngine`] (feature `engine-usearch`) wraps the USearch 2.26
//!   C++11 HNSW core (cxx FFI, `L2sq` metric with `U8` quantization) with sync,
//!   thread-safe methods; `open` loads the index file for read-write.
//!
//! Inputs are ready-made `(chunk_id, Vec<f32>)` pairs; `chunk_id` is the application-level
//! primary key (the SQLite chunk row id). The dimensionality is a configuration parameter -
//! this crate never calls the embedding crate, so the query path does not load the model.
//!
//! # Cascade protocol (design D3)
//!
//! The LanceDB index and the SQLite chunk table are separate stores with no
//! cross-database transaction. Consistency is a three-layer protocol owned by
//! the consumer:
//!
//! 1. **Cascade order (call contract).** When removing chunks, the consumer
//!    calls [`VectorIndex::delete_by_chunk_ids`] BEFORE deleting the chunk
//!    rows in SQLite (the oracle's "vectors → chunks" order). A crash between
//!    the steps leaves orphaned *vectors* — visible and machine-detectable by
//!    reconciliation — instead of live chunks with missing vectors, which
//!    would silently degrade recall.
//! 2. **Consumer tolerance.** Search returns chunk ids; the consumer filters
//!    results against SQLite, so an orphaned vector can never reach the user
//!    even before garbage collection runs.
//! 3. **Reconciliation primitives.** [`VectorIndex::chunk_ids`] +
//!    [`VectorIndex::count`] let a GC job compute "index − SQLite → delete".
//!    The ultimate repair is a full [`VectorIndex::rebuild`] from chunk text
//!    (re-encoding done by the ingestion layer, a future change).
//!
//! Module [`synx`] implements the SYNX binary fixture format (`vectors.bin`), the
//! oracle ↔ harness vector-dump contract (native-seam-spikes design D4): a streaming
//! reader and a chunk_id-sorted writer.

#[cfg(feature = "engine-lance")]
pub mod engine;
pub mod error;
pub mod synx;
#[cfg(feature = "engine-usearch")]
pub mod usearch_engine;

#[cfg(feature = "engine-lance")]
pub use engine::LanceEngine;
pub use error::VectorsError;
#[cfg(feature = "engine-usearch")]
pub use usearch_engine::UsearchEngine;

use std::path::Path;
use std::sync::Arc;

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

    /// Atomically replaces the entire content with `rows` and rebuilds the
    /// ANN index over them: on success only `rows` are stored — nothing from
    /// the previous content remains (no accumulation). An empty `rows`
    /// empties the index. The ultimate repair of the cascade protocol
    /// (design D3).
    fn rebuild(&self, rows: &[(u32, Vec<f32>)]) -> Result<(), VectorsError>;
}

/// The LanceDB engine name (feature `engine-lance`; the ADR 0003 default).
///
/// Engine names are accepted by the `vectors.engine` config field and by
/// [`create_vector_engine`] (add-usearch-ann-engine, design.md "Runtime").
/// The config crate validates the same two literals at parse time (it cannot
/// depend on this crate — dependency direction D1), so these constants are
/// the factory's reference spelling.
pub const ENGINE_LANCE: &str = "lance";
/// The USearch engine name (feature `engine-usearch`).
pub const ENGINE_USEARCH: &str = "usearch";

/// The concrete ANN engine behind the [`VectorIndex`] seam, selected at
/// runtime by the `vectors.engine` config field (add-usearch-ann-engine,
/// design.md "Dispatch").
///
/// Each variant exists only when its cargo feature is compiled in
/// (`engine-lance` / `engine-usearch`); a build without either feature has
/// no variants and [`create_vector_engine`] always fails.
pub enum VectorEngine {
    /// LanceDB engine (feature `engine-lance`; the ADR 0003 default).
    #[cfg(feature = "engine-lance")]
    Lance(LanceEngine),
    /// USearch engine (feature `engine-usearch`; U8-quantized disk-backed
    /// HNSW, `L2sq`).
    #[cfg(feature = "engine-usearch")]
    Usearch(UsearchEngine),
}

// With neither engine feature the enum has no variants and no `VectorIndex`
// impl: `create_vector_engine` always fails and the type is uninstantiable.
#[cfg(any(feature = "engine-lance", feature = "engine-usearch"))]
impl VectorEngine {
    /// The inner engine's [`VectorIndex`] implementation (pure delegation).
    fn inner(&self) -> &dyn VectorIndex {
        match self {
            #[cfg(feature = "engine-lance")]
            Self::Lance(engine) => engine,
            #[cfg(feature = "engine-usearch")]
            Self::Usearch(engine) => engine,
        }
    }
}

#[cfg(any(feature = "engine-lance", feature = "engine-usearch"))]
impl VectorIndex for VectorEngine {
    fn insert(&self, chunk_id: u32, vector: &[f32]) -> Result<(), VectorsError> {
        self.inner().insert(chunk_id, vector)
    }

    fn insert_batch(&self, rows: &[(u32, &[f32])]) -> Result<(), VectorsError> {
        self.inner().insert_batch(rows)
    }

    fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u32, f32)>, VectorsError> {
        self.inner().search(query, k)
    }

    fn delete_by_chunk_ids(&self, chunk_ids: &[u32]) -> Result<(), VectorsError> {
        self.inner().delete_by_chunk_ids(chunk_ids)
    }

    fn chunk_ids(&self) -> Result<Vec<u32>, VectorsError> {
        self.inner().chunk_ids()
    }

    fn count(&self) -> Result<u64, VectorsError> {
        self.inner().count()
    }

    fn build_index(&self) -> Result<(), VectorsError> {
        self.inner().build_index()
    }

    fn rebuild(&self, rows: &[(u32, Vec<f32>)]) -> Result<(), VectorsError> {
        self.inner().rebuild(rows)
    }
}

/// Creates the ANN engine selected by `engine_name` at `path`, opening an
/// existing index or creating an empty one on first run (the
/// `open` → `NotFound` → `create` cascade both engines share).
///
/// `engine_name` is the resolved `vectors.engine` value: [`ENGINE_LANCE`]
/// or [`ENGINE_USEARCH`]; an empty name (absent config field) resolves to
/// the default engine [`ENGINE_LANCE`] (backward compatibility).
///
/// # Errors
///
/// - [`VectorsError::InvalidArgument`] for an unrecognized engine name or a
///   `config` that fails [`VectorIndexConfig::validate`].
/// - [`VectorsError::Engine`] when the name is recognized but no engine
///   feature is compiled into this build.
/// - Engine errors (including [`VectorsError::DimensionMismatch`] when the
///   stored index's dimensionality disagrees with `config.dim`) propagate
///   unchanged.
pub fn create_vector_engine(
    engine_name: &str,
    path: &Path,
    config: &VectorIndexConfig,
) -> Result<Arc<dyn VectorIndex>, VectorsError> {
    let name = if engine_name.is_empty() {
        ENGINE_LANCE
    } else {
        engine_name
    };
    // Validated before dispatch: the engines re-validate in create/open, so
    // the error is identical — this just fails earlier for a bad config.
    config.validate()?;

    if name != ENGINE_LANCE && name != ENGINE_USEARCH {
        return Err(VectorsError::InvalidArgument(format!(
            "unknown vector engine '{name}', want \"{ENGINE_LANCE}\" or \"{ENGINE_USEARCH}\""
        )));
    }

    #[cfg(feature = "engine-lance")]
    if name == ENGINE_LANCE {
        let engine = match LanceEngine::open(path, *config) {
            Ok(engine) => engine,
            Err(VectorsError::NotFound(_)) => LanceEngine::create(path, *config)?,
            Err(err) => return Err(err),
        };
        return Ok(Arc::new(VectorEngine::Lance(engine)));
    }

    #[cfg(feature = "engine-usearch")]
    if name == ENGINE_USEARCH {
        let engine = match UsearchEngine::open(path, *config) {
            Ok(engine) => engine,
            Err(VectorsError::NotFound(_)) => UsearchEngine::create(path, *config)?,
            Err(err) => return Err(err),
        };
        return Ok(Arc::new(VectorEngine::Usearch(engine)));
    }

    // A known engine name, but no engine feature was compiled in.
    Err(VectorsError::Engine(format!(
        "engine '{name}' is not available in this build for {} (rebuild with the matching engine feature)",
        path.display()
    )))
}

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (fixtures always parse).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

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

    // --- create_vector_engine (task 1.3) -----------------------------------

    /// A unique temporary directory that removes itself (and its contents)
    /// when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "synopsis-vectors-test-{}-{tag}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Unwraps a failed [`create_vector_engine`] result (`Arc<dyn VectorIndex>`
    /// is not `Debug`, so `expect_err` is unavailable).
    fn err_of(result: Result<Arc<dyn VectorIndex>, VectorsError>) -> VectorsError {
        match result {
            Err(err) => err,
            Ok(_) => panic!("expected an error, got a created engine"),
        }
    }

    #[cfg(feature = "engine-lance")]
    #[test]
    fn create_vector_engine_lance_selects_lance_engine() {
        let dir = TempDir::new("factory-lance");
        let config = VectorIndexConfig::default();

        let engine = create_vector_engine(ENGINE_LANCE, &dir.0, &config).expect("lance engine");
        // The on-disk artifact is the LanceDB table directory inside the
        // engine path (LanceDB local-storage layout) — the fingerprint that
        // the LanceEngine was selected.
        assert!(
            dir.0.join("vectors.lance").exists(),
            "the lance table directory must be created"
        );
        assert_eq!(
            engine.count().expect("count"),
            0,
            "empty index on first run"
        );

        // First run created the index; a second factory call must OPEN it
        // (the cascade), not reset it — both engines' `create` fails on an
        // existing index, so success proves the open path.
        let reopened = create_vector_engine(ENGINE_LANCE, &dir.0, &config).expect("reopen");
        assert_eq!(
            reopened.count().expect("count"),
            0,
            "empty index on first run"
        );
    }

    #[cfg(feature = "engine-usearch")]
    #[test]
    fn create_vector_engine_usearch_selects_usearch_engine() {
        let dir = TempDir::new("factory-usearch");
        let config = VectorIndexConfig::default();

        let engine = create_vector_engine(ENGINE_USEARCH, &dir.0, &config).expect("usearch engine");
        // The on-disk artifact is the single index file inside the engine
        // directory (add-usearch-ann-engine design.md layout) — the
        // fingerprint that the UsearchEngine was selected.
        assert!(
            dir.0.join("index.usearch").exists(),
            "the usearch index file must be created"
        );
        assert_eq!(
            engine.count().expect("count"),
            0,
            "empty index on first run"
        );

        let reopened = create_vector_engine(ENGINE_USEARCH, &dir.0, &config).expect("reopen");
        assert_eq!(
            reopened.count().expect("count"),
            0,
            "empty index on first run"
        );
    }

    #[cfg(feature = "engine-lance")]
    #[test]
    fn create_vector_engine_empty_name_defaults_to_lance() {
        let dir = TempDir::new("factory-default");
        let config = VectorIndexConfig::default();
        let engine = create_vector_engine("", &dir.0, &config).expect("default engine");
        // The LanceDB table directory is the fingerprint that the default
        // engine (lance) was selected.
        assert!(
            dir.0.join("vectors.lance").exists(),
            "an absent engine name must resolve to the lance default"
        );
        assert_eq!(
            engine.count().expect("count"),
            0,
            "empty index on first run"
        );
    }

    #[test]
    fn create_vector_engine_unknown_name_is_invalid_argument() {
        let dir = TempDir::new("factory-unknown");
        let config = VectorIndexConfig::default();
        let err = err_of(create_vector_engine("foo", &dir.0, &config));
        match err {
            VectorsError::InvalidArgument(message) => {
                assert!(
                    message.contains("foo"),
                    "the error must name the value: {message}"
                );
            }
            other => panic!("expected InvalidArgument, got: {other:?}"),
        }
    }

    #[cfg(all(feature = "engine-lance", not(feature = "engine-usearch")))]
    #[test]
    fn create_vector_engine_usearch_unavailable_in_lance_only_build() {
        let dir = TempDir::new("factory-unavailable");
        let config = VectorIndexConfig::default();
        let err = err_of(create_vector_engine(ENGINE_USEARCH, &dir.0, &config));
        match err {
            VectorsError::Engine(message) => {
                assert!(
                    message.contains("not available in this build"),
                    "the error must name the missing feature: {message}"
                );
            }
            other => panic!("expected Engine, got: {other:?}"),
        }
    }

    #[cfg(all(feature = "engine-usearch", not(feature = "engine-lance")))]
    #[test]
    fn create_vector_engine_lance_unavailable_in_usearch_only_build() {
        let dir = TempDir::new("factory-unavailable");
        let config = VectorIndexConfig::default();
        let err = err_of(create_vector_engine(ENGINE_LANCE, &dir.0, &config));
        match err {
            VectorsError::Engine(message) => {
                assert!(
                    message.contains("not available in this build"),
                    "the error must name the missing feature: {message}"
                );
            }
            other => panic!("expected Engine, got: {other:?}"),
        }
    }
}
