//! ANN index contract (trait) and the USearch engine; replaces brute-force
//! vector search. Tier 0 per design.md D1: no internal dependencies; the sole
//! engine is USearch (ADR 0004).
//!
//! The public seam is [`VectorIndex`] (object-safe; consumers hold
//! `Arc<dyn VectorIndex>`), [`VectorIndexConfig`] (index/query parameters) and
//! [`VectorsError`]. One engine implements the trait:
//!
//! - [`usearch::UsearchEngine`] wraps the USearch 2.26 C++11 HNSW core (cxx FFI,
//!   `L2sq` metric with configurable scalar quantization, default `BF16`) with
//!   sync, thread-safe methods; `open` loads the index file for read-write.
//!
//! Inputs are ready-made `(chunk_id, Vec<f32>)` pairs; `chunk_id` is the application-level
//! primary key (the SQLite chunk row id). The dimensionality is a configuration parameter -
//! this crate never calls the embedding crate, so the query path does not load the model.
//!
//! # Cascade protocol (design D3)
//!
//! The vector index and the SQLite chunk table are separate stores with no
//! cross-database transaction. Consistency is a three-layer protocol owned by
//! the consumer:
//!
//! 1. **Cascade order (call contract).** When removing chunks, the consumer
//!    calls [`VectorIndex::delete_by_chunk_ids`] BEFORE deleting the chunk
//!    rows in SQLite (the "vectors → chunks" order). A crash between
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
//! Module [`synx`] implements the SYNX binary fixture format (`vectors.bin`) —
//! the vector-dump fixture contract (native-seam-spikes design D4): a streaming
//! reader and a chunk_id-sorted writer.

pub mod error;
pub mod synx;
pub mod usearch;

/// Test-only seams for integration tests (change test-hygiene-phase-2
/// task 2.6); docs-hidden, not part of the public API.
#[doc(hidden)]
pub mod test_support;

pub use error::VectorsError;
pub use usearch::UsearchEngine;

use std::path::Path;
use std::sync::Arc;

/// usearch engine tuning (usearch-wal-persistence task 2.1): segment size
/// after compaction, the stale-vector percentage that triggers compaction,
/// and the number of parallel search threads (rayon).
///
/// Mirrors `config::preset::UsearchConfig` (dependency direction D1: this
/// crate cannot depend on `config`), so the wiring maps it field by field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsearchConfig {
    /// Maximum vectors per segment after compaction.
    pub max_segment_vectors: usize,
    /// Stale-vector percentage (1-100) that triggers compaction.
    pub compaction_stale_threshold: u8,
    /// Number of parallel search threads (rayon).
    pub search_threads: usize,
}

impl Default for UsearchConfig {
    /// Engine defaults: 1M vectors per segment, 30% stale threshold, 4
    /// search threads (usearch-wal-persistence design).
    fn default() -> Self {
        Self {
            max_segment_vectors: 1_000_000,
            compaction_stale_threshold: 30,
            search_threads: 4,
        }
    }
}

impl UsearchConfig {
    /// Validates field invariants:
    ///
    /// - `max_segment_vectors > 0`
    /// - `compaction_stale_threshold` in 1..=100
    /// - `search_threads > 0`
    pub fn validate(&self) -> Result<(), VectorsError> {
        if self.max_segment_vectors == 0 {
            return Err(VectorsError::InvalidArgument(
                "usearch.max_segment_vectors must be > 0".to_string(),
            ));
        }
        if !(1..=100).contains(&self.compaction_stale_threshold) {
            return Err(VectorsError::InvalidArgument(format!(
                "usearch.compaction_stale_threshold must be in 1..=100, got {}",
                self.compaction_stale_threshold
            )));
        }
        if self.search_threads == 0 {
            return Err(VectorsError::InvalidArgument(
                "usearch.search_threads must be > 0".to_string(),
            ));
        }
        Ok(())
    }
}

/// Index and query parameters for the ANN engine.
///
/// Defaults are the ADR 0004 configuration (HNSW, `bf16` scalar quantization,
/// `L2sq`): `dim = 1024` (bge-m3), `m = 16`, `ef_construction = 100`,
/// `ef_search = 200`. `ef_search` is runtime-tunable: raising it trades latency
/// for recall without rebuilding the index.
///
/// `quantization` is the engine's scalar quantization. It is `None` by default,
/// which the engine resolves to its default (`bf16`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorIndexConfig {
    /// Vector dimensionality (bge-m3: 1024).
    pub dim: usize,
    /// HNSW graph degree M.
    pub m: usize,
    /// HNSW efConstruction: candidate list size during index build.
    pub ef_construction: usize,
    /// HNSW efSearch: candidate list size during query.
    pub ef_search: usize,
    /// Scalar quantization for the ANN index: one of `"u8"`, `"i8"`, `"f16"`,
    /// `"bf16"`, `"f32"`. `None` means the engine default (`bf16`).
    pub quantization: Option<String>,
    /// Engine tuning (WAL segments + compaction, usearch-wal-persistence
    /// task 2.1): `None` means the engine defaults ([`UsearchConfig::default`]).
    pub usearch: Option<UsearchConfig>,
}

impl VectorIndexConfig {
    /// Creates a configuration, validating all field invariants.
    pub fn new(
        dim: usize,
        m: usize,
        ef_construction: usize,
        ef_search: usize,
    ) -> Result<Self, VectorsError> {
        let config = Self {
            dim,
            m,
            ef_construction,
            ef_search,
            quantization: None,
            usearch: None,
        };
        config.validate()?;
        Ok(config)
    }

    /// Sets the scalar quantization (builder). `None` (the default from
    /// [`Self::new`]) means the engine's default quantization (`bf16`).
    pub fn with_quantization(mut self, quantization: impl Into<String>) -> Self {
        self.quantization = Some(quantization.into());
        self
    }

    /// Validates field invariants:
    ///
    /// - `dim > 0`
    /// - `m > 0`
    /// - `ef_construction >= m` (HNSW invariant)
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
        if self.ef_search == 0 {
            return Err(VectorsError::InvalidArgument(
                "ef_search must be > 0".to_string(),
            ));
        }
        // The quantization is engine-specific (usearch); validate the value if
        // set so a programmatic misuse fails here, not deep in the engine.
        if let Some(quantization) = &self.quantization {
            match quantization.to_ascii_lowercase().as_str() {
                "u8" | "i8" | "f16" | "bf16" | "f32" => {}
                other => {
                    return Err(VectorsError::InvalidArgument(format!(
                        "quantization must be one of \"u8\", \"i8\", \"f16\", \"bf16\", \"f32\", got {other:?}"
                    )));
                }
            }
        }
        // The usearch tuning section is engine-specific; validate it if set
        // (usearch-wal-persistence task 2.1).
        if let Some(usearch) = &self.usearch {
            usearch.validate()?;
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
    /// ADR 0004 configuration: 1024-dim, M=16, efConstruction=100, efSearch=200.
    /// `usearch` stays `None` (the engine defaults, usearch-wal-persistence
    /// task 2.1).
    fn default() -> Self {
        Self {
            dim: 1024,
            m: 16,
            ef_construction: 100,
            ef_search: 200,
            quantization: None,
            usearch: None,
        }
    }
}

/// Disk-backed approximate nearest-neighbour index keyed by chunk id.
///
/// The trait is object-safe (`Send + Sync`, no generics or `async fn`): consumers hold
/// `Arc<dyn VectorIndex>`. All methods are sync (a disk-backed index with blocking
/// I/O): implementations must be called only from sync contexts or
/// `spawn_blocking` workers, never from inside an async task.
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

    /// Stores many vectors in one call; the engine batches them (~1000 rows
    /// per chunk) for concurrent insertion.
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
    /// (HNSW per ADR 0004).
    fn build_index(&self) -> Result<(), VectorsError>;

    /// Atomically replaces the entire content with `rows` and rebuilds the
    /// ANN index over them: on success only `rows` are stored — nothing from
    /// the previous content remains (no accumulation). An empty `rows`
    /// empties the index. The ultimate repair of the cascade protocol
    /// (design D3).
    fn rebuild(&self, rows: &[(u32, Vec<f32>)]) -> Result<(), VectorsError>;

    /// Best-effort background compaction (usearch-wal-persistence task 3.8,
    /// ADR 0004 §7): merges the stale DISK segments into fresh ones when
    /// the stale-vector fraction crosses the engine's configured threshold.
    ///
    /// The default is a no-op; the engine overrides it with the ADR 0004 §7
    /// background repack. The call returns promptly — the repack runs on a
    /// background thread, never blocking the query path.
    fn maybe_compact(&self) -> Result<(), VectorsError> {
        Ok(())
    }
}

/// The USearch engine name (the sole engine; also the default).
///
/// Engine names are accepted by the `vectors.engine` config field and by
/// [`create_vector_engine`]. The config crate validates the same literal at
/// parse time (it cannot depend on this crate — dependency direction D1), so
/// this constant is the factory's reference spelling.
pub const ENGINE_USEARCH: &str = "usearch";

/// Creates the ANN engine at `path`, opening an existing index or creating an
/// empty one on first run (the `open` → `NotFound` → `create` cascade).
///
/// `engine_name` is the resolved `vectors.engine` value: [`ENGINE_USEARCH`]
/// or an empty name (absent config field), both of which select the sole
/// engine. The removed engine name is rejected with
/// [`VectorsError::EngineRemoved`]; any other name is an
/// [`VectorsError::InvalidArgument`].
///
/// The engine stores its index in its own subdirectory of `path`:
/// `<path>/usearch` (the `DatasetConfig::vectors_engine_path` layout). The
/// factory is the single place that resolves that subdirectory, so an
/// existing dataset directory is never orphaned by an engine switch.
///
/// `wal_db` is the knowledge database path holding the
/// `usearch_vectors_log` WAL table (ADR 0004 §3, usearch-wal-persistence
/// task 3.9): the engine opens a dedicated long-lived connection to it and
/// journals its mutations there (WAL-first); `None` disables the WAL
/// (RAM-only behavior). The `config.usearch` tuning section (ADR 0004 §10)
/// travels with the index config; an absent section resolves to the engine
/// defaults.
///
/// # Errors
///
/// - [`VectorsError::EngineRemoved`] when `engine_name` is the removed
///   engine name.
/// - [`VectorsError::InvalidArgument`] for an unrecognized engine name or a
///   `config` that fails [`VectorIndexConfig::validate`].
/// - Engine errors (including [`VectorsError::DimensionMismatch`] when the
///   stored index's dimensionality disagrees with `config.dim`) propagate
///   unchanged.
pub fn create_vector_engine(
    engine_name: &str,
    path: &Path,
    config: &VectorIndexConfig,
    wal_db: Option<&Path>,
) -> Result<Arc<dyn VectorIndex>, VectorsError> {
    let name = if engine_name.is_empty() {
        ENGINE_USEARCH
    } else {
        engine_name
    };
    // Validated before dispatch: the engine re-validates in create/open, so
    // the error is identical — this just fails earlier for a bad config.
    config.validate()?;

    if name == "lance" {
        return Err(VectorsError::EngineRemoved);
    }
    if name != ENGINE_USEARCH {
        return Err(VectorsError::InvalidArgument(format!(
            "unknown vector engine '{name}', want \"{ENGINE_USEARCH}\""
        )));
    }

    // The engine-tagged storage subdirectory (task 1.5 layout).
    let engine_path = path.join(ENGINE_USEARCH);

    // ADR 0004 §10 (usearch-wal-persistence task 3.9): the `usearch` tuning
    // section travels with the index config; an absent section resolves to
    // the engine defaults. Both create and open read it from `config.usearch`
    // (Revision 1, task 3.10), so the factory passes nothing extra —
    // `create_with_wal` is symmetric with `open_with_wal`.
    let engine = match UsearchEngine::open_with_wal(&engine_path, config.clone(), wal_db) {
        Ok(engine) => engine,
        Err(VectorsError::NotFound(_)) => {
            UsearchEngine::create_with_wal(&engine_path, config.clone(), wal_db)?
        }
        Err(err) => return Err(err),
    };
    Ok(Arc::new(engine))
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
    fn defaults_match_adr_0004() {
        let config = VectorIndexConfig::default();
        assert_eq!(config.dim, 1024);
        assert_eq!(config.m, 16);
        assert_eq!(config.ef_construction, 100);
        assert_eq!(config.ef_search, 200);
        // `None` means the engine default (bf16 for usearch).
        assert_eq!(config.quantization, None);
        config.validate().expect("defaults must be valid");
    }

    #[test]
    fn new_accepts_valid_config() {
        let config = VectorIndexConfig::new(512, 8, 64, 100).unwrap();
        assert_eq!(config.dim, 512);
        assert_eq!(config.ef_construction, 64);
        assert_eq!(config.quantization, None);
    }

    #[test]
    fn with_quantization_sets_the_field() {
        let config = VectorIndexConfig::new(512, 8, 64, 100)
            .unwrap()
            .with_quantization("f32");
        assert_eq!(config.quantization.as_deref(), Some("f32"));
        config.validate().expect("f32 quantization is valid");
    }

    #[test]
    fn validate_rejects_invalid_quantization() {
        assert!(
            config_with(|c| c.quantization = Some("fp8".to_string()))
                .validate()
                .is_err()
        );
        // Every accepted value validates.
        for value in ["u8", "i8", "f16", "bf16", "f32"] {
            let config = config_with(|c| c.quantization = Some(value.to_string()));
            config
                .validate()
                .unwrap_or_else(|err| panic!("{value} must validate: {err:?}"));
        }
        // Case is tolerated.
        assert!(
            config_with(|c| c.quantization = Some("BF16".to_string()))
                .validate()
                .is_ok()
        );
    }

    // --- UsearchConfig (usearch-wal-persistence task 2.1) ------------------

    #[test]
    fn usearch_config_defaults_match_design() {
        let config = UsearchConfig::default();
        assert_eq!(config.max_segment_vectors, 1_000_000);
        assert_eq!(config.compaction_stale_threshold, 30);
        assert_eq!(config.search_threads, 4);
        config.validate().expect("defaults must be valid");
    }

    #[test]
    fn usearch_config_validate_rejects_invalid_values() {
        let zero_segments = UsearchConfig {
            max_segment_vectors: 0,
            ..UsearchConfig::default()
        };
        assert!(zero_segments.validate().is_err());

        let zero_threshold = UsearchConfig {
            compaction_stale_threshold: 0,
            ..UsearchConfig::default()
        };
        assert!(zero_threshold.validate().is_err());

        let over_threshold = UsearchConfig {
            compaction_stale_threshold: 101,
            ..UsearchConfig::default()
        };
        assert!(over_threshold.validate().is_err());

        let zero_threads = UsearchConfig {
            search_threads: 0,
            ..UsearchConfig::default()
        };
        assert!(zero_threads.validate().is_err());
    }

    #[test]
    fn vector_index_config_defaults_have_no_usearch_section() {
        assert_eq!(VectorIndexConfig::default().usearch, None);
        assert_eq!(
            VectorIndexConfig::new(512, 8, 64, 100)
                .expect("valid")
                .usearch,
            None
        );
    }

    #[test]
    fn vector_index_config_validate_covers_usearch_section() {
        let config = VectorIndexConfig {
            usearch: Some(UsearchConfig {
                max_segment_vectors: 0,
                ..UsearchConfig::default()
            }),
            ..VectorIndexConfig::default()
        };
        assert!(config.validate().is_err());

        let config = VectorIndexConfig {
            usearch: Some(UsearchConfig::default()),
            ..VectorIndexConfig::default()
        };
        config
            .validate()
            .expect("a valid usearch section must pass");
    }

    #[test]
    fn validate_rejects_invalid_fields() {
        assert!(config_with(|c| c.dim = 0).validate().is_err());
        assert!(config_with(|c| c.m = 0).validate().is_err());
        // ef_construction < m violates the HNSW invariant.
        assert!(config_with(|c| c.ef_construction = 10).validate().is_err());
        assert!(config_with(|c| c.ef_search = 0).validate().is_err());
    }

    #[test]
    fn new_propagates_validation_errors() {
        assert!(VectorIndexConfig::new(0, 16, 100, 200).is_err());
        assert!(VectorIndexConfig::new(1024, 16, 10, 200).is_err());
        assert!(VectorIndexConfig::new(1024, 0, 100, 200).is_err());
        assert!(VectorIndexConfig::new(1024, 16, 100, 0).is_err());
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

    #[test]
    fn create_vector_engine_removed_engine_name_is_engine_removed() {
        // The removed engine name is rejected with the dedicated,
        // actionable `EngineRemoved` error (not `InvalidArgument`).
        let dir = TempDir::new("factory-removed");
        let config = VectorIndexConfig::default();
        let err = err_of(create_vector_engine("lance", &dir.0, &config, None));
        match err {
            VectorsError::EngineRemoved => {}
            other => panic!("expected EngineRemoved, got: {other:?}"),
        }
    }

    #[test]
    fn create_vector_engine_usearch_selects_usearch_engine() {
        let dir = TempDir::new("factory-usearch");
        let config = VectorIndexConfig::default();

        let engine =
            create_vector_engine(ENGINE_USEARCH, &dir.0, &config, None).expect("usearch engine");
        // The on-disk artifact is the ADR 0004 §1 layout inside the
        // engine-tagged subdirectory (task 3.3) — the fingerprint that the
        // UsearchEngine was selected.
        assert!(
            dir.0.join("usearch").join("ram.keys").exists(),
            "the usearch RAM manifest must be created under the usearch subdirectory"
        );
        assert_eq!(
            engine.count().expect("count"),
            0,
            "empty index on first run"
        );

        let reopened = create_vector_engine(ENGINE_USEARCH, &dir.0, &config, None).expect("reopen");
        assert_eq!(
            reopened.count().expect("count"),
            0,
            "empty index on first run"
        );
    }

    #[test]
    fn create_vector_engine_empty_name_defaults_to_usearch() {
        let dir = TempDir::new("factory-default");
        let config = VectorIndexConfig::default();
        let engine = create_vector_engine("", &dir.0, &config, None).expect("default engine");
        // The usearch RAM manifest under the usearch subdirectory is the
        // fingerprint that the default engine (usearch) was selected.
        assert!(
            dir.0.join("usearch").join("ram.keys").exists(),
            "an absent engine name must resolve to the usearch default"
        );
        assert_eq!(
            engine.count().expect("count"),
            0,
            "empty index on first run"
        );
    }

    #[test]
    fn usearch_dimension_mismatch_at_engine_tagged_path() {
        // The stored-index dimension check fires at the engine-tagged path
        // (512-dim stored vs 1024-dim configured).
        let dir = TempDir::new("usearch-dim-mismatch");
        let stored = VectorIndexConfig::new(512, 16, 100, 200).expect("stored config");
        let engine =
            create_vector_engine(ENGINE_USEARCH, &dir.0, &stored, None).expect("create index");
        // Task 3.3 (ADR 0004 layout): the dim check fires against stored
        // files, so the layout needs a saved 512-dim snapshot first.
        engine
            .insert(1, &vec![0.5f32; 512])
            .expect("insert a 512-dim row");
        engine.build_index().expect("save the snapshot");

        let err = err_of(create_vector_engine(
            ENGINE_USEARCH,
            &dir.0,
            &VectorIndexConfig::default(),
            None,
        ));
        match err {
            VectorsError::DimensionMismatch { expected, actual } => {
                assert_eq!((expected, actual), (1024, 512));
            }
            other => panic!("expected DimensionMismatch, got: {other:?}"),
        }
    }

    #[test]
    fn create_vector_engine_unknown_name_is_invalid_argument() {
        let dir = TempDir::new("factory-unknown");
        let config = VectorIndexConfig::default();
        let err = err_of(create_vector_engine("foo", &dir.0, &config, None));
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

    #[test]
    fn factory_wires_wal_db_and_usearch_section() {
        // Task 3.9 (ADR 0004 §9/§10): the factory passes the WAL database
        // path and the `usearch` tuning section into the engine — the
        // defect #2/#3 wiring.
        use rusqlite::Connection;

        let dir = TempDir::new("factory-wal");
        // The knowledge database: the WAL table (migrations 3+4 shape).
        let db_path = dir.0.join("knowledge.db");
        let conn = Connection::open(&db_path).expect("open wal db");
        conn.execute(
            "CREATE TABLE usearch_vectors_log (
                segment_id INTEGER NOT NULL,
                chunk_id INTEGER NOT NULL,
                flags INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                PRIMARY KEY (segment_id, chunk_id)
            )",
            [],
        )
        .expect("create wal table");
        drop(conn);

        // The usearch section is applied (defect #3): `max_segment_vectors =
        // 42` means the 42-row batch overflows the RAM layer into segment-1
        // (the engine default of 1M would never flush).
        let mut config = VectorIndexConfig::new(8, 4, 8, 8).expect("test config");
        config.usearch = Some(UsearchConfig {
            max_segment_vectors: 42,
            ..UsearchConfig::default()
        });

        let engine = create_vector_engine(ENGINE_USEARCH, &dir.0, &config, Some(db_path.as_path()))
            .expect("factory with the WAL db");
        let vector = vec![0.5f32; config.dim];
        for id in 1..=42u32 {
            engine.insert(id, &vector).expect("insert");
        }
        assert!(
            dir.0
                .join("usearch")
                .join("segments")
                .join("segment-1.usearch")
                .is_file(),
            "the 42-row batch must overflow into segment-1 (max_segment_vectors = 42)"
        );

        // The WAL is wired end-to-end (defect #2): a fresh engine on the
        // same db + dir (the open cascade) sees the saved state, and a
        // re-insert of a DISK key journals the supersession row.
        drop(engine);
        let reopened =
            create_vector_engine(ENGINE_USEARCH, &dir.0, &config, Some(db_path.as_path()))
                .expect("reopen on the same db + dir");
        assert_eq!(
            reopened.count().expect("count"),
            42,
            "the flushed data must survive the restart"
        );
        reopened.insert(1, &vector).expect("re-insert a DISK key");

        let conn = Connection::open(&db_path).expect("reopen wal db");
        let rows: Vec<(i64, i64)> = {
            let mut stmt = conn
                .prepare("SELECT segment_id, chunk_id FROM usearch_vectors_log")
                .expect("prepare wal rows");
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .expect("query wal rows")
                .collect::<Result<_, _>>()
                .expect("collect wal rows")
        };
        assert_eq!(
            rows,
            vec![(1, 1)],
            "the supersession row for the DISK key must be journaled"
        );
    }
}
