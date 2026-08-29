//! USearch-backed ANN engine - SPIKE (add-usearch-ann-engine, task 1.1).
//!
//! Proves the `usearch` 2.26 crate (C++11 HNSW core behind cxx FFI) compiles
//! in this workspace and that its core API works at the target geometry:
//! 1024-dim vectors, `L2sq` metric, `U8` quantization (design.md).
//!
//! Verified API facts (usearch 2.26.1, docs.rs + crate source):
//!
//! - `Index::new(&IndexOptions { dimensions, metric, quantization,
//!   connectivity, expansion_add, expansion_search, multi })` creates the
//!   index; the field is named `quantization` (not `dtype`);
//! - `add`/`search` accept `&[f32]` on a `U8`-quantized index: the C++ core
//!   down-casts every input vector itself (`u8[i] = clamp(v[i] * 255 / ||v||,
//!   0, 255)` - per-vector L2 normalization before the 8-bit scale), so the
//!   Rust side always passes f32;
//! - `save(path)` / `Index::restore_view(path)` round-trip the index; the
//!   view mmap's the file (disk-backed serving, no full RAM load);
//! - keys are `u64` (`usearch::Key`), distances are `f32`, results arrive as
//!   `Matches { keys, distances }` sorted by distance ascending;
//! - `Index` is `Send + Sync` and every mutating method takes `&self` - the
//!   C++ index is concurrent by design;
//! - `reserve(capacity)` MUST be called before the first `add`: in 2.26 an
//!   insertion that finds no reserved worker thread fails with "Reserve
//!   capacity ahead of insertions!". Reserve grows capacity (it never
//!   shrinks), so reserving the final size up front is the efficient pattern
//!   (task 1.2: `insert_batch`/`rebuild` reserve the row count first).
//!
//! This module is a stub: the full [`crate::VectorIndex`] implementation
//! lands in task 1.2. Gated on the `engine-usearch` feature.

use std::path::{Path, PathBuf};

use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

use crate::VectorsError;

/// Vector dimensionality of the spike (bge-m3, per design.md).
const DIM: usize = 1024;
/// HNSW graph degree (design.md parity parameters).
const CONNECTIVITY: usize = 16;
/// HNSW candidate list size during index build (design.md).
const EXPANSION_ADD: usize = 100;
/// HNSW candidate list size during query (design.md).
const EXPANSION_SEARCH: usize = 200;
/// Single on-disk index file inside the engine directory (design.md layout:
/// `<vectors_path>/usearch/`).
const INDEX_FILE: &str = "index.usearch";

/// Spike stub of the USearch engine (task 1.1): owns a usearch `Index` and
/// the on-disk file it was created or restored from.
///
/// `Send + Sync` (both via `usearch::Index`); all mutation goes through
/// `&self` because the C++ index is concurrent by design.
pub struct UsearchEngine {
    index: Index,
    /// The `<dir>/index.usearch` file backing the index.
    path: PathBuf,
}

impl UsearchEngine {
    /// Creates a fresh 1024-dim `L2sq`/`U8` index in `dir` (creating the
    /// directory if needed) and persists the empty index to
    /// `<dir>/index.usearch`. Fails if that file already exists - use
    /// [`Self::open`] for that.
    pub fn create(dir: impl Into<PathBuf>) -> Result<Self, VectorsError> {
        let dir = dir.into();
        let path = dir.join(INDEX_FILE);
        if path.exists() {
            return Err(VectorsError::Engine(format!(
                "index already exists at {}",
                path.display()
            )));
        }
        std::fs::create_dir_all(&dir)?;
        let index = Index::new(&options()).map_err(map_usearch)?;
        let engine = Self { index, path };
        engine.save()?;
        Ok(engine)
    }

    /// Opens a previously saved index at `dir` as a read-only mmap view
    /// (disk-backed: vectors are paged in from the file, not loaded into
    /// RAM). Returns [`VectorsError::NotFound`] if the index file is absent.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, VectorsError> {
        let dir = dir.into();
        let path = dir.join(INDEX_FILE);
        if !path.exists() {
            return Err(VectorsError::NotFound(dir.display().to_string()));
        }
        let index = Index::restore_view(&to_str(&path)?).map_err(map_usearch)?;
        Ok(Self { index, path })
    }

    /// Reserves capacity for `capacity` vectors (and the default number of
    /// worker threads). Must be called before the first [`Self::add`] - the
    /// C++ core rejects insertions without reserved capacity. Re-reserving
    /// with a smaller value is a no-op (capacity never shrinks).
    pub fn reserve(&self, capacity: usize) -> Result<(), VectorsError> {
        self.index.reserve(capacity).map_err(map_usearch)
    }

    /// Adds `vector` under `key`. The vector length must equal the index
    /// dimensionality; the C++ core down-casts f32 to the U8 storage type
    /// itself. Requires prior [`Self::reserve`] with enough capacity.
    pub fn add(&self, key: u64, vector: &[f32]) -> Result<(), VectorsError> {
        check_dim(vector.len(), self.index.dimensions())?;
        self.index.add(key, vector).map_err(map_usearch)
    }

    /// Top-k nearest neighbours of `query` as `(key, distance)` pairs sorted
    /// by distance ascending. An empty index yields an empty vec (not an
    /// error). Requires `k > 0` and `query.len() == dim`.
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32)>, VectorsError> {
        check_dim(query.len(), self.index.dimensions())?;
        if k == 0 {
            return Err(VectorsError::InvalidArgument(
                "top-k must be > 0".to_string(),
            ));
        }
        let matches = self.index.search(query, k).map_err(map_usearch)?;
        Ok(matches.keys.into_iter().zip(matches.distances).collect())
    }

    /// Persists the index to the on-disk file.
    pub fn save(&self) -> Result<(), VectorsError> {
        self.index.save(&to_str(&self.path)?).map_err(map_usearch)
    }

    /// Number of vectors currently stored.
    pub fn size(&self) -> u64 {
        self.index.size() as u64
    }

    /// The on-disk index file backing this engine.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Spike index options: 1024-dim, `L2sq` metric, `U8` quantization, HNSW
/// connectivity 16 / expansion_add 100 / expansion_search 200 (design.md).
fn options() -> IndexOptions {
    IndexOptions {
        dimensions: DIM,
        metric: MetricKind::L2sq,
        quantization: ScalarKind::U8,
        connectivity: CONNECTIVITY,
        expansion_add: EXPANSION_ADD,
        expansion_search: EXPANSION_SEARCH,
        multi: false,
    }
}

/// Maps a usearch cxx FFI exception to [`VectorsError::Engine`].
fn map_usearch(err: cxx::Exception) -> VectorsError {
    VectorsError::Engine(format!("usearch: {err}"))
}

/// Rejects a vector whose length differs from the index dimensionality
/// (before it reaches the C++ core).
fn check_dim(actual: usize, expected: usize) -> Result<(), VectorsError> {
    if actual != expected {
        return Err(VectorsError::DimensionMismatch { expected, actual });
    }
    Ok(())
}

/// usearch FFI paths are C strings: the path must be valid UTF-8.
fn to_str(path: &Path) -> Result<String, VectorsError> {
    path.to_str().map(str::to_string).ok_or_else(|| {
        VectorsError::InvalidArgument(format!("path is not valid UTF-8: {}", path.display()))
    })
}

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (test infra always succeeds).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    /// A unique temporary directory that removes itself on drop (the same
    /// pattern as `engine::tests::TempDir`; `tempfile` is not in the palette).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "synopsis-vectors-usearch-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A deterministic seeded vector: SplitMix64-driven uniform f32 in
    /// [-1, 1) (the same construction as the LanceEngine tests).
    fn seeded_vector(seed: u64, dim: usize) -> Vec<f32> {
        let mut state = seed;
        (0..dim)
            .map(|_| {
                state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = state;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^= z >> 31;
                (z >> 40) as f32 / (1 << 23) as f32 - 1.0
            })
            .collect()
    }

    /// Acceptance criterion (task 1.1): create -> add 100 vectors dim=1024 ->
    /// search top-10 -> save -> restore(view) -> search gives the same keys.
    #[test]
    fn create_add_search_save_restore_view_roundtrip() {
        let dir = TempDir::new();
        let engine = UsearchEngine::create(&dir.0).expect("create engine");
        assert_eq!(engine.size(), 0, "fresh index is empty");
        assert_eq!(engine.index.dimensions(), DIM);
        assert_eq!(engine.index.metric_kind(), MetricKind::L2sq);
        assert_eq!(engine.index.scalar_kind(), ScalarKind::U8);

        // Reserve before the first add (required by the 2.26 C++ core).
        engine.reserve(100).expect("reserve");

        // 100 deterministic 1024-dim vectors, one per key.
        let vectors: Vec<(u64, Vec<f32>)> =
            (0..100).map(|key| (key, seeded_vector(key, DIM))).collect();
        for (key, vector) in &vectors {
            engine.add(*key, vector).expect("add vector");
        }
        assert_eq!(engine.size(), 100);

        // Top-10 for a stored vector: that vector is top-1 at ~0 distance
        // (the query is down-cast to U8 exactly like its stored copy) and
        // the results are distance-ascending.
        let results = engine.search(&vectors[42].1, 10).expect("search");
        assert_eq!(results.len(), 10, "10 results for 100 stored vectors");
        assert_eq!(results[0].0, 42, "top-1 must be the queried vector");
        assert!(
            results[0].1 < 1e-3,
            "top-1 distance must be ~0, got {}",
            results[0].1
        );
        for pair in results.windows(2) {
            assert!(
                pair[0].1 <= pair[1].1,
                "results must be distance-asc: {results:?}"
            );
        }

        engine.save().expect("save");
        drop(engine);

        // Restore as a read-only mmap view: the same top-10 keys come back.
        let view = UsearchEngine::open(&dir.0).expect("open view");
        assert_eq!(view.size(), 100, "view sees the saved vectors");
        let restored = view.search(&vectors[42].1, 10).expect("search on view");
        let keys = |results: &[(u64, f32)]| results.iter().map(|(key, _)| *key).collect::<Vec<_>>();
        assert_eq!(
            keys(&results),
            keys(&restored),
            "restore(view) must return the same keys"
        );
    }

    #[test]
    fn open_missing_index_is_not_found() {
        let dir = TempDir::new();
        let err = match UsearchEngine::open(&dir.0) {
            Err(err) => err,
            Ok(_) => panic!("opening a missing index must fail"),
        };
        match err {
            VectorsError::NotFound(path) => {
                assert_eq!(
                    path,
                    dir.0.display().to_string(),
                    "payload is the looked-up dir"
                )
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn create_on_existing_index_fails() {
        let dir = TempDir::new();
        UsearchEngine::create(&dir.0).expect("first create");
        let err = match UsearchEngine::create(&dir.0) {
            Err(err) => err,
            Ok(_) => panic!("second create must fail"),
        };
        assert!(matches!(err, VectorsError::Engine(_)), "got {err:?}");
    }

    #[test]
    fn dim_mismatch_and_zero_k_are_rejected() {
        let dir = TempDir::new();
        let engine = UsearchEngine::create(&dir.0).expect("create");
        // Search (like add) needs a reserved worker thread, even on an empty
        // index.
        engine.reserve(1).expect("reserve");
        let short = vec![0.0f32; 512];
        assert!(
            matches!(
                engine.add(1, &short),
                Err(VectorsError::DimensionMismatch {
                    expected: 1024,
                    actual: 512
                })
            ),
            "short add must be rejected"
        );
        assert!(
            matches!(
                engine.search(&short, 5),
                Err(VectorsError::DimensionMismatch { .. })
            ),
            "short search must be rejected"
        );
        let good = seeded_vector(1, DIM);
        assert!(
            matches!(
                engine.search(&good, 0),
                Err(VectorsError::InvalidArgument(_))
            ),
            "zero-k search must be rejected"
        );
        // An empty index searches to an empty result, not an error.
        assert!(
            engine
                .search(&good, 5)
                .expect("search on empty index")
                .is_empty()
        );
    }
}
