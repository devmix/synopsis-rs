//! LanceDB-backed ANN engine (tasks vectors 1.3/1.4; design D2/D3/D4, ADR 0003).
//!
//! [`LanceEngine`] implements the full [`crate::VectorIndex`] contract on
//! LanceDB local-filesystem storage:
//!
//! - table create/open with the schema `chunk_id: UInt32` +
//!   `vector: FixedSizeList<Float32, dim>` (design D4);
//! - batched Arrow inserts (~1000 rows per batch, design D4);
//! - [`LanceEngine::build_index`] — `IvfHnswSq` (u8 scalar quantization, L2
//!   metric) with the parameters from [`VectorIndexConfig`]; raw f32 vectors
//!   stay in the table, quantization lives only inside the index;
//! - [`LanceEngine::search`] — top-k by L2 distance ascending;
//! - lifecycle operations (task vectors 1.4): [`LanceEngine::delete_by_chunk_ids`],
//!   [`LanceEngine::chunk_ids`], [`LanceEngine::count`], [`LanceEngine::rebuild`] —
//!   the SQLite-reconciliation primitives of the cascade protocol (design D3,
//!   documented at the crate root).
//!
//! # Sync facade (design D2)
//!
//! The LanceDB API is fully async, so the engine owns a dedicated tokio
//! runtime (multi-thread, 2 workers) and executes it through `block_on`.
//! Callers MUST invoke the engine only from sync contexts or
//! `tokio::task::spawn_blocking` workers — never from inside an async task,
//! where a nested `block_on` is not allowed. The engine is `Send + Sync` and
//! safe to share across `spawn_blocking` workers.
//!
//! # Errors
//!
//! - opening a path without an index → [`VectorsError::NotFound`];
//! - a vector whose length differs from `config.dim` (insert, search, or a
//!   stored table with a different dim on open) →
//!   [`VectorsError::DimensionMismatch`];
//! - searching an empty index → empty result, not an error.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt;
use lancedb::arrow::arrow_array::{FixedSizeListArray, Float32Array, RecordBatch, UInt32Array};
use lancedb::arrow::arrow_schema::{DataType as ArrowDataType, Field, Schema as ArrowSchema};
use lancedb::index::Index;
use lancedb::index::vector::IvfHnswSqIndexBuilder;
use lancedb::query::{ExecutableQuery, QueryBase, Select};
use lancedb::table::AddDataMode;
use lancedb::{DistanceType, Table, connect};
use tokio::runtime::{Builder, Runtime};

use crate::{VectorIndex, VectorIndexConfig, VectorsError};

/// LanceDB table name; the on-disk table directory is `<data_dir>/vectors.lance`.
const TABLE_NAME: &str = "vectors";
/// Chunk-id column (application-level primary key; uniqueness is the caller's).
const COL_CHUNK_ID: &str = "chunk_id";
/// Vector column.
const COL_VECTOR: &str = "vector";
/// Distance column added to vector-search results by lance.
const COL_DISTANCE: &str = "_distance";
/// Arrow batch size for inserts (design D4).
const BATCH_ROWS: usize = 1000;
/// Chunk-id limit per `delete` transaction (one LanceDB commit per batch).
const DELETE_BATCH_IDS: usize = 1000;
/// Workers of the dedicated tokio runtime (design D2: at most 2).
const RUNTIME_WORKERS: usize = 2;

/// LanceDB-backed ANN index (tasks vectors 1.3/1.4).
///
/// Owns a dedicated tokio runtime (design D2): all methods are sync and must
/// be called only from sync contexts or `spawn_blocking` workers. The
/// instance is `Send + Sync` and can be shared across threads.
///
/// Implements [`VectorIndex`] by pure delegation to the inherent methods,
/// so `Arc<dyn VectorIndex>` and `LanceEngine` are interchangeable.
///
/// See the module docs for the table schema and error semantics.
pub struct LanceEngine {
    config: VectorIndexConfig,
    runtime: Runtime,
    /// Data directory holding the LanceDB database.
    path: PathBuf,
    table: Table,
}

impl LanceEngine {
    /// Creates a new engine with an EMPTY table at `path` (creating the
    /// directory if needed). Fails if a table already exists there — use
    /// [`Self::open`] for that.
    pub fn create(
        path: impl Into<PathBuf>,
        config: VectorIndexConfig,
    ) -> Result<Self, VectorsError> {
        let path = path.into();
        config.validate()?;
        std::fs::create_dir_all(&path)?;
        let runtime = new_runtime()?;
        let table = runtime.block_on(async {
            let conn = connect(to_uri(&path)?.as_str())
                .execute()
                .await
                .map_err(map_lance)?;
            conn.create_empty_table(TABLE_NAME, arrow_schema(&config))
                .execute()
                .await
                .map_err(map_lance)
        })?;
        Ok(Self {
            config,
            runtime,
            path,
            table,
        })
    }

    /// Opens an engine at `path` created earlier by [`Self::create`].
    ///
    /// Returns [`VectorsError::NotFound`] if no index exists at `path`, and
    /// [`VectorsError::DimensionMismatch`] if the stored table's vector
    /// dimensionality differs from `config.dim`.
    pub fn open(path: impl Into<PathBuf>, config: VectorIndexConfig) -> Result<Self, VectorsError> {
        let path = path.into();
        config.validate()?;
        let runtime = new_runtime()?;
        let table = runtime.block_on(async {
            let conn = connect(to_uri(&path)?.as_str())
                .execute()
                .await
                .map_err(map_lance)?;
            match conn.open_table(TABLE_NAME).execute().await {
                Ok(table) => Ok(table),
                Err(lancedb::Error::TableNotFound { .. }) => {
                    Err(VectorsError::NotFound(path.display().to_string()))
                }
                Err(err) => Err(map_lance(err)),
            }
        })?;
        let actual = stored_dim(&table, &runtime)?;
        if actual != config.dim {
            return Err(VectorsError::DimensionMismatch {
                expected: config.dim,
                actual,
            });
        }
        Ok(Self {
            config,
            runtime,
            path,
            table,
        })
    }

    /// Stores `vector` under `chunk_id` (single-row convenience over
    /// [`Self::insert_batch`]). The vector length must equal `config.dim`.
    pub fn insert(&self, chunk_id: u32, vector: &[f32]) -> Result<(), VectorsError> {
        self.insert_batch(&[(chunk_id, vector)])
    }

    /// Stores many vectors; rows are chunked into Arrow batches of
    /// [`BATCH_ROWS`] (design D4) and appended in one write. Every vector must
    /// have length `config.dim` (otherwise [`VectorsError::DimensionMismatch`],
    /// and the whole call is rejected). Uniqueness of `chunk_id` is the
    /// caller's responsibility.
    pub fn insert_batch(&self, rows: &[(u32, &[f32])]) -> Result<(), VectorsError> {
        if rows.is_empty() {
            return Ok(());
        }
        let dim = self.config.dim;
        for &(_, vector) in rows {
            if vector.len() != dim {
                return Err(VectorsError::DimensionMismatch {
                    expected: dim,
                    actual: vector.len(),
                });
            }
        }
        let config = self.config;
        let batches: Vec<RecordBatch> = rows
            .chunks(BATCH_ROWS)
            .map(|chunk| build_batch(&config, chunk))
            .collect::<Result<Vec<_>, _>>()?;
        self.runtime
            .block_on(self.table.add(batches).execute())
            .map(|_| ())
            .map_err(map_lance)
    }

    /// Top-k nearest neighbours of `query`: `(chunk_id, distance)` pairs
    /// sorted by L2 distance ascending. An empty index yields an empty vec
    /// (not an error). Requires `k > 0` and `query.len() == config.dim`.
    ///
    /// The query uses `config.nprobes` IVF partitions and `config.ef_search`
    /// HNSW beam width (runtime-tunable per ADR 0003 mitigation #1: raising
    /// them trades latency for recall without rebuilding the index).
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u32, f32)>, VectorsError> {
        self.config.validate_search(query, k)?;
        let nprobes = self.config.nprobes;
        let ef_search = self.config.ef_search;
        self.runtime.block_on(async {
            let mut stream = self
                .table
                .vector_search(query)
                .map_err(map_lance)?
                .select(Select::columns(&[COL_CHUNK_ID]))
                .nprobes(nprobes)
                .ef(ef_search)
                .limit(k)
                .execute()
                .await
                .map_err(map_lance)?;
            let mut results: Vec<(u32, f32)> = Vec::with_capacity(k);
            while let Some(batch) = stream.next().await {
                let batch = batch.map_err(map_lance)?;
                let ids = batch
                    .column_by_name(COL_CHUNK_ID)
                    .and_then(|col| col.as_any().downcast_ref::<UInt32Array>())
                    .ok_or_else(|| {
                        VectorsError::Engine(format!(
                            "search result lacks the {COL_CHUNK_ID} column"
                        ))
                    })?;
                let distances = batch
                    .column_by_name(COL_DISTANCE)
                    .and_then(|col| col.as_any().downcast_ref::<Float32Array>())
                    .ok_or_else(|| {
                        VectorsError::Engine(format!(
                            "search result lacks the {COL_DISTANCE} column"
                        ))
                    })?;
                for (id, distance) in ids.iter().zip(distances.iter()) {
                    if let (Some(id), Some(distance)) = (id, distance) {
                        results.push((id, distance));
                    }
                }
            }
            // The SDK does not guarantee stream row order: sort explicitly.
            results.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
            Ok(results)
        })
    }

    /// Builds the `IvfHnswSq` index over the stored vectors with the
    /// configured parameters (M, efConstruction, num_partitions; L2 metric).
    /// Call it once the data is in place; rebuilding over new data is
    /// handled with the lifecycle operations (task vectors 1.4).
    pub fn build_index(&self) -> Result<(), VectorsError> {
        let config = self.config;
        let index = Index::IvfHnswSq(
            IvfHnswSqIndexBuilder::default()
                .num_partitions(config.num_partitions as u32)
                .num_edges(config.m as u32)
                .ef_construction(config.ef_construction as u32)
                .distance_type(DistanceType::L2),
        );
        self.runtime
            .block_on(self.table.create_index(&[COL_VECTOR], index).execute())
            .map_err(map_lance)
    }

    /// Removes the vectors whose chunk id is in `chunk_ids`.
    ///
    /// Idempotent: ids that are not present are silently ignored (a LanceDB
    /// delete is a predicate-based no-op when it matches nothing), so calling
    /// it again with the same ids — or after a crash between the vector
    /// delete and the SQLite chunk delete (design D3) — is always safe. Id
    /// lists longer than [`DELETE_BATCH_IDS`] are split into one LanceDB
    /// transaction per batch.
    pub fn delete_by_chunk_ids(&self, chunk_ids: &[u32]) -> Result<(), VectorsError> {
        for batch in chunk_ids.chunks(DELETE_BATCH_IDS) {
            let predicate = format!(
                "{} IN ({})",
                COL_CHUNK_ID,
                batch
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            self.runtime
                .block_on(self.table.delete(&predicate))
                .map(|_| ())
                .map_err(map_lance)?;
        }
        Ok(())
    }

    /// All chunk ids currently stored, in no particular order.
    ///
    /// Reconciliation primitive (design D3): the GC job computes
    /// "index − SQLite → delete" from this listing. The query itself streams;
    /// the returned vec holds one `u32` per stored row (≈4 MB at N=1M).
    pub fn chunk_ids(&self) -> Result<Vec<u32>, VectorsError> {
        self.runtime.block_on(async {
            let mut stream = self
                .table
                .query()
                .select(Select::columns(&[COL_CHUNK_ID]))
                .execute()
                .await
                .map_err(map_lance)?;
            let mut ids: Vec<u32> = Vec::new();
            while let Some(batch) = stream.next().await {
                let batch = batch.map_err(map_lance)?;
                let column = batch.column_by_name(COL_CHUNK_ID).ok_or_else(|| {
                    VectorsError::Engine(format!(
                        "chunk_id query result lacks the {COL_CHUNK_ID} column"
                    ))
                })?;
                let array = column
                    .as_any()
                    .downcast_ref::<UInt32Array>()
                    .ok_or_else(|| {
                        VectorsError::Engine(format!(
                            "the {COL_CHUNK_ID} column is not a UInt32 array"
                        ))
                    })?;
                ids.extend(array.iter().flatten());
            }
            Ok(ids)
        })
    }

    /// Number of vectors currently stored (deleted rows are not counted).
    pub fn count(&self) -> Result<u64, VectorsError> {
        let rows = self
            .runtime
            .block_on(self.table.count_rows(None))
            .map_err(map_lance)?;
        Ok(rows as u64)
    }

    /// Atomically replaces the entire content with `rows` and rebuilds the
    /// index.
    ///
    /// Strategy (verified against lancedb 0.37): one `add` in
    /// [`AddDataMode::Overwrite`] mode. LanceDB commits the replacement as a
    /// single versioned dataset write, so a crash mid-rebuild leaves the
    /// previous content intact (the operation is simply re-run) — there is no
    /// drop/recreate window with a missing or stale index. Lance drops the
    /// vector index on overwrite, so the index is rebuilt with
    /// [`Self::build_index`] immediately after the write.
    ///
    /// Every vector must have length `config.dim` (the whole call is rejected
    /// otherwise, before anything is written). An empty `rows` slice empties
    /// the index (no index is built over zero rows — lance does not support
    /// it, and an empty table needs none). This is the ultimate repair of the
    /// cascade protocol (design D3): rebuild from chunk text after the
    /// embedding crate re-encodes the chunks.
    pub fn rebuild(&self, rows: &[(u32, Vec<f32>)]) -> Result<(), VectorsError> {
        let dim = self.config.dim;
        for (_, vector) in rows {
            if vector.len() != dim {
                return Err(VectorsError::DimensionMismatch {
                    expected: dim,
                    actual: vector.len(),
                });
            }
        }
        let config = self.config;
        let refs: Vec<(u32, &[f32])> = rows
            .iter()
            .map(|(id, vector)| (*id, vector.as_slice()))
            .collect();
        let batches: Vec<RecordBatch> = if refs.is_empty() {
            // The overwrite still needs a batch to carry the schema; a
            // zero-row batch empties the table.
            vec![build_batch(&config, &[])?]
        } else {
            refs.chunks(BATCH_ROWS)
                .map(|chunk| build_batch(&config, chunk))
                .collect::<Result<Vec<_>, _>>()?
        };
        self.runtime
            .block_on(
                self.table
                    .add(batches)
                    .mode(AddDataMode::Overwrite)
                    .execute(),
            )
            .map(|_| ())
            .map_err(map_lance)?;
        // Lance cannot build an IvfHnswSq index over zero rows ("Creating
        // empty vector indices ... is not yet implemented"); an empty table
        // needs no index — search flat-scans and returns nothing.
        if !refs.is_empty() {
            self.build_index()?;
        }
        Ok(())
    }

    /// The index/query configuration this engine was created with.
    pub fn config(&self) -> &VectorIndexConfig {
        &self.config
    }

    /// The data directory holding the index on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Pure delegation to the inherent methods, which carry the full
/// documentation. Fully-qualified `LanceEngine::method` calls make the
/// delegation unambiguous (inherent methods shadow trait methods, but the
/// explicit form keeps the intent readable).
impl VectorIndex for LanceEngine {
    fn insert(&self, chunk_id: u32, vector: &[f32]) -> Result<(), VectorsError> {
        LanceEngine::insert(self, chunk_id, vector)
    }

    fn insert_batch(&self, rows: &[(u32, &[f32])]) -> Result<(), VectorsError> {
        LanceEngine::insert_batch(self, rows)
    }

    fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u32, f32)>, VectorsError> {
        LanceEngine::search(self, query, k)
    }

    fn delete_by_chunk_ids(&self, chunk_ids: &[u32]) -> Result<(), VectorsError> {
        LanceEngine::delete_by_chunk_ids(self, chunk_ids)
    }

    fn chunk_ids(&self) -> Result<Vec<u32>, VectorsError> {
        LanceEngine::chunk_ids(self)
    }

    fn count(&self) -> Result<u64, VectorsError> {
        LanceEngine::count(self)
    }

    fn build_index(&self) -> Result<(), VectorsError> {
        LanceEngine::build_index(self)
    }

    fn rebuild(&self, rows: &[(u32, Vec<f32>)]) -> Result<(), VectorsError> {
        LanceEngine::rebuild(self, rows)
    }
}

/// The dedicated tokio runtime (design D2): multi-thread with 2 workers; the
/// IO and time drivers are enabled for LanceDB's async I/O.
fn new_runtime() -> Result<Runtime, VectorsError> {
    Builder::new_multi_thread()
        .worker_threads(RUNTIME_WORKERS)
        .enable_io()
        .enable_time()
        .build()
        .map_err(|err| VectorsError::Engine(format!("create runtime: {err}")))
}

/// Maps a LanceDB error to [`VectorsError`]: a missing table becomes
/// [`VectorsError::NotFound`] (named by the table), everything else a generic
/// engine error.
fn map_lance(err: lancedb::Error) -> VectorsError {
    match err {
        lancedb::Error::TableNotFound { name, .. } => VectorsError::NotFound(name),
        other => VectorsError::Engine(other.to_string()),
    }
}

/// The Arrow schema of the index table (design D4).
fn arrow_schema(config: &VectorIndexConfig) -> Arc<ArrowSchema> {
    Arc::new(ArrowSchema::new(vec![
        Field::new(COL_CHUNK_ID, ArrowDataType::UInt32, false),
        Field::new(
            COL_VECTOR,
            ArrowDataType::FixedSizeList(
                Arc::new(Field::new("item", ArrowDataType::Float32, true)),
                config.dim as i32,
            ),
            false,
        ),
    ]))
}

/// Builds one Arrow batch from a slice of rows (all rows must already have
/// `config.dim` length; the caller pre-validates).
fn build_batch(
    config: &VectorIndexConfig,
    rows: &[(u32, &[f32])],
) -> Result<RecordBatch, VectorsError> {
    let ids = UInt32Array::from_iter_values(rows.iter().map(|(id, _)| *id));
    let values = Float32Array::from(
        rows.iter()
            .flat_map(|(_, vector)| vector.iter().copied())
            .collect::<Vec<f32>>(),
    );
    let list = FixedSizeListArray::try_new(
        Arc::new(Field::new("item", ArrowDataType::Float32, true)),
        config.dim as i32,
        Arc::new(values),
        None,
    )
    .map_err(|err| VectorsError::Engine(format!("build vector column: {err}")))?;
    RecordBatch::try_new(arrow_schema(config), vec![Arc::new(ids), Arc::new(list)])
        .map_err(|err| VectorsError::Engine(format!("build record batch: {err}")))
}

/// The vector dimensionality of a stored table.
fn stored_dim(table: &Table, runtime: &Runtime) -> Result<usize, VectorsError> {
    runtime.block_on(async {
        let wrapper = table
            .dataset()
            .ok_or_else(|| VectorsError::Engine("table has no local dataset".to_string()))?;
        let dataset = wrapper.get().await.map_err(map_lance)?;
        let field = dataset
            .schema()
            .field(COL_VECTOR)
            .ok_or_else(|| VectorsError::Engine(format!("table lacks the {COL_VECTOR} column")))?;
        match field.data_type() {
            ArrowDataType::FixedSizeList(_, dim) => Ok(dim as usize),
            other => Err(VectorsError::Engine(format!(
                "the {COL_VECTOR} column is not a fixed-size list: {other:?}"
            ))),
        }
    })
}

/// A `connect` URI for a local data directory.
fn to_uri(path: &Path) -> Result<String, VectorsError> {
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
    /// pattern as `db::test_util::TempDb`; `tempfile` is not in the palette).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "synopsis-vectors-test-{}-{}",
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
    /// [-1, 1). No RNG crate needed (frozen palette); determinism is all the
    /// tests require (queries are exact copies of stored rows).
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

    /// ADR 0003 dim with small-corpus index parameters (reduced partitions
    /// per the task acceptance criteria).
    fn test_config() -> VectorIndexConfig {
        VectorIndexConfig::new(1024, 16, 100, 8, 8, 100).expect("test config is valid")
    }

    /// `n` seeded rows with ids 0..n at dim 1024.
    fn rows(n: usize) -> Vec<(u32, Vec<f32>)> {
        (0..n as u32)
            .map(|id| (id, seeded_vector(0xA11CE + id as u64, 1024)))
            .collect()
    }

    fn batch_refs(data: &[(u32, Vec<f32>)]) -> Vec<(u32, &[f32])> {
        data.iter().map(|(id, vec)| (*id, vec.as_slice())).collect()
    }

    #[test]
    fn create_insert_build_search_roundtrip() {
        let dir = TempDir::new();
        let engine = LanceEngine::create(&dir.0, test_config()).expect("create engine");

        let data = rows(2500); // spans 3 Arrow batches of 1000 rows
        engine
            .insert_batch(&batch_refs(&data))
            .expect("insert 2500 rows");
        engine.build_index().expect("build IvfHnswSq index");

        // Query = an exact stored row: top-1 must be that row at ~0 distance,
        // and the results must be sorted by distance ascending.
        let results = engine.search(&data[123].1, 10).expect("search");
        assert_eq!(results.len(), 10, "10 results for 2500 stored rows");
        assert_eq!(results[0].0, 123, "top-1 must be the queried row");
        assert!(
            results[0].1 < 1e-4,
            "top-1 distance must be ~0, got {}",
            results[0].1
        );
        for pair in results.windows(2) {
            assert!(
                pair[0].1 <= pair[1].1,
                "results must be distance-asc: {results:?}"
            );
        }

        // A single-row insert after the index build lands and is searchable
        // (lance combines the indexed search with a flat scan of new rows).
        let extra = seeded_vector(0xDEAD_BEEF, 1024);
        engine.insert(9999, &extra).expect("single insert");
        let results = engine.search(&extra, 5).expect("search after insert");
        assert_eq!(results[0].0, 9999, "freshly inserted row must be top-1");
    }

    #[test]
    fn reopen_without_rebuild_sees_data() {
        let dir = TempDir::new();
        let data = rows(500);
        let query = data[7].1.clone();

        let engine = LanceEngine::create(&dir.0, test_config()).expect("create");
        engine.insert_batch(&batch_refs(&data)).expect("insert");
        engine.build_index().expect("build");
        let before = engine.search(&query, 5).expect("search");
        drop(engine);

        let engine = LanceEngine::open(&dir.0, test_config()).expect("reopen");
        let after = engine.search(&query, 5).expect("search");
        let ids = |results: &[(u32, f32)]| results.iter().map(|(id, _)| *id).collect::<Vec<_>>();
        assert_eq!(
            ids(&before),
            ids(&after),
            "reopen without rebuild must see the same data"
        );
        assert_eq!(after[0].0, 7, "top-1 must still be the queried row");
    }

    /// `LanceEngine` is not `Debug` (it owns a tokio runtime), so a failing
    /// construction is unwrapped by hand instead of `expect_err`.
    fn err_of(result: Result<LanceEngine, VectorsError>) -> VectorsError {
        match result {
            Err(err) => err,
            Ok(_) => panic!("engine construction must fail"),
        }
    }

    #[test]
    fn open_missing_index_is_not_found() {
        let dir = TempDir::new();
        let err = err_of(LanceEngine::open(&dir.0, test_config()));
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
    fn create_on_existing_table_fails() {
        let dir = TempDir::new();
        LanceEngine::create(&dir.0, test_config()).expect("first create");
        let err = err_of(LanceEngine::create(&dir.0, test_config()));
        assert!(matches!(err, VectorsError::Engine(_)), "got {err:?}");
    }

    #[test]
    fn dim_mismatch_is_rejected() {
        let dir = TempDir::new();
        let engine = LanceEngine::create(&dir.0, test_config()).expect("create");

        let short = vec![0.0f32; 512];
        match engine.insert(1, &short) {
            Err(VectorsError::DimensionMismatch { expected, actual }) => {
                assert_eq!((expected, actual), (1024, 512));
            }
            other => panic!("expected DimensionMismatch, got {other:?}"),
        }
        match engine.search(&short, 5) {
            Err(VectorsError::DimensionMismatch { .. }) => {}
            other => panic!("expected DimensionMismatch, got {other:?}"),
        }

        // A batch with one bad row is rejected as a whole.
        let good = seeded_vector(1, 1024);
        let bad = vec![0.0f32; 3];
        let batch = vec![(1u32, good.as_slice()), (2u32, bad.as_slice())];
        assert!(
            matches!(
                engine.insert_batch(&batch),
                Err(VectorsError::DimensionMismatch { .. })
            ),
            "mixed batch must be rejected"
        );
    }

    #[test]
    fn search_empty_index_returns_empty() {
        let dir = TempDir::new();
        let engine = LanceEngine::create(&dir.0, test_config()).expect("create");
        let query = seeded_vector(42, 1024);
        let results = engine.search(&query, 10).expect("search on empty index");
        assert!(
            results.is_empty(),
            "empty index must yield no results, got {results:?}"
        );
    }

    #[test]
    fn open_with_different_dim_is_rejected() {
        let dir = TempDir::new();
        let small = VectorIndexConfig::new(512, 16, 100, 8, 8, 100).expect("valid");
        LanceEngine::create(&dir.0, small).expect("create with dim 512");

        let err = err_of(LanceEngine::open(&dir.0, test_config()));
        match err {
            VectorsError::DimensionMismatch { expected, actual } => {
                assert_eq!((expected, actual), (1024, 512));
            }
            other => panic!("expected DimensionMismatch, got {other:?}"),
        }
    }

    #[test]
    fn delete_removes_rows_from_search() {
        let dir = TempDir::new();
        let engine = LanceEngine::create(&dir.0, test_config()).expect("create");
        let data = rows(500);
        engine.insert_batch(&batch_refs(&data)).expect("insert");
        engine.build_index().expect("build");

        let to_delete = [7u32, 100, 250, 499];
        engine.delete_by_chunk_ids(&to_delete).expect("delete");

        let results = engine.search(&data[7].1, 100).expect("search after delete");
        let ids = results.iter().map(|(id, _)| *id).collect::<Vec<_>>();
        for &gone in &to_delete {
            assert!(
                !ids.contains(&gone),
                "deleted id {gone} must not be returned"
            );
        }
        assert_eq!(engine.count().expect("count"), 496);
    }

    #[test]
    fn delete_is_idempotent_for_absent_ids() {
        let dir = TempDir::new();
        let engine = LanceEngine::create(&dir.0, test_config()).expect("create");
        let data = rows(100);
        engine.insert_batch(&batch_refs(&data)).expect("insert");
        engine.build_index().expect("build");

        let ids = [5u32, 50];
        engine.delete_by_chunk_ids(&ids).expect("first delete");
        engine
            .delete_by_chunk_ids(&ids)
            .expect("repeat delete of the same ids");
        // Ids that were never stored, plus an empty list (a no-op).
        engine
            .delete_by_chunk_ids(&[9000, 9001])
            .expect("delete of absent ids");
        engine.delete_by_chunk_ids(&[]).expect("empty delete");

        assert_eq!(engine.count().expect("count"), 98);
        let remaining = engine.chunk_ids().expect("chunk_ids");
        assert!(remaining.iter().all(|id| !ids.contains(id)));
    }

    /// More ids than one [`DELETE_BATCH_IDS`] transaction exercises the
    /// batched-delete path (2 commits for 1200 ids).
    #[test]
    fn delete_batches_long_id_lists() {
        let dir = TempDir::new();
        let engine = LanceEngine::create(&dir.0, test_config()).expect("create");
        let data = rows(1500);
        engine.insert_batch(&batch_refs(&data)).expect("insert");
        engine.build_index().expect("build");

        let to_delete: Vec<u32> = (0..1200).collect();
        engine
            .delete_by_chunk_ids(&to_delete)
            .expect("batched delete");
        assert_eq!(engine.count().expect("count"), 300);
        let remaining = engine.chunk_ids().expect("chunk_ids");
        assert!(remaining.iter().all(|id| *id >= 1200));
    }

    #[test]
    fn chunk_ids_and_count_track_insert_and_delete() {
        let dir = TempDir::new();
        let engine = LanceEngine::create(&dir.0, test_config()).expect("create");
        assert_eq!(engine.count().expect("count"), 0);
        assert!(engine.chunk_ids().expect("chunk_ids").is_empty());

        let data = rows(1200); // spans 2 Arrow batches
        engine.insert_batch(&batch_refs(&data)).expect("insert");
        assert_eq!(engine.count().expect("count"), 1200);
        let mut ids = engine.chunk_ids().expect("chunk_ids");
        ids.sort_unstable();
        let expected: Vec<u32> = (0..1200).collect();
        assert_eq!(ids, expected, "every stored id exactly once");

        engine.delete_by_chunk_ids(&[0, 599, 1199]).expect("delete");
        assert_eq!(engine.count().expect("count"), 1197);
        let ids = engine.chunk_ids().expect("chunk_ids");
        assert_eq!(ids.len(), 1197);
        assert!(!ids.contains(&0) && !ids.contains(&599) && !ids.contains(&1199));
    }

    #[test]
    fn rebuild_replaces_content_without_accumulation() {
        let dir = TempDir::new();
        let engine = LanceEngine::create(&dir.0, test_config()).expect("create");
        let first = rows(300);
        engine
            .insert_batch(&batch_refs(&first))
            .expect("insert first");
        engine.build_index().expect("build first");

        let second: Vec<(u32, Vec<f32>)> = (300..600)
            .map(|id| (id, seeded_vector(0xBEEF + id as u64, 1024)))
            .collect();
        engine.rebuild(&second).expect("rebuild");

        // No accumulation: exactly the new rows, none of the first set.
        assert_eq!(engine.count().expect("count"), 300);
        let ids = engine.chunk_ids().expect("chunk_ids");
        assert_eq!(ids.len(), 300);
        assert!(ids.iter().all(|id| (300..600).contains(id)));

        // The index was rebuilt: a new row is top-1 for its own vector, and a
        // query equal to a replaced (old) row must not return the old id.
        let results = engine.search(&second[100].1, 5).expect("search new");
        assert_eq!(results[0].0, 400, "new row 300+100 must be top-1");
        let stale = engine.search(&first[0].1, 10).expect("search stale query");
        assert!(
            !stale.iter().any(|(id, _)| *id < 300),
            "old ids must be gone: {stale:?}"
        );
    }

    /// Empirically pins the zero-row overwrite path (empty rebuild).
    #[test]
    fn rebuild_with_empty_rows_empties_the_index() {
        let dir = TempDir::new();
        let engine = LanceEngine::create(&dir.0, test_config()).expect("create");
        let data = rows(100);
        engine.insert_batch(&batch_refs(&data)).expect("insert");
        engine.build_index().expect("build");

        engine.rebuild(&[]).expect("rebuild to empty");
        assert_eq!(engine.count().expect("count"), 0);
        assert!(engine.chunk_ids().expect("chunk_ids").is_empty());
        let query = seeded_vector(1, 1024);
        assert!(engine.search(&query, 5).expect("search").is_empty());
    }

    #[test]
    fn reopen_after_lifecycle_ops_sees_everything() {
        let dir = TempDir::new();
        let data = rows(400);
        let query = data[5].1.clone();

        let engine = LanceEngine::create(&dir.0, test_config()).expect("create");
        engine.insert_batch(&batch_refs(&data)).expect("insert");
        engine.build_index().expect("build");
        engine.delete_by_chunk_ids(&[5, 137]).expect("delete");
        drop(engine);

        let engine = LanceEngine::open(&dir.0, test_config()).expect("reopen");
        assert_eq!(engine.count().expect("count"), 398);
        let ids = engine.chunk_ids().expect("chunk_ids");
        assert!(!ids.contains(&5) && !ids.contains(&137));
        let results = engine.search(&query, 100).expect("search");
        assert!(!results.iter().any(|(id, _)| *id == 5 || *id == 137));

        // A rebuild on the reopened engine persists as well.
        let replacement = rows(50);
        engine.rebuild(&replacement).expect("rebuild");
        drop(engine);
        let engine = LanceEngine::open(&dir.0, test_config()).expect("reopen after rebuild");
        assert_eq!(engine.count().expect("count"), 50);
        let ids = engine.chunk_ids().expect("chunk_ids");
        assert!(ids.iter().all(|id| *id < 50));
    }

    /// The full contract exercised through `&dyn VectorIndex` — proves the
    /// delegation impl and the object safety of the trait.
    #[test]
    fn trait_object_delegates_to_engine() {
        let dir = TempDir::new();
        let engine = LanceEngine::create(&dir.0, test_config()).expect("create");
        let data = rows(200);
        let index: &dyn VectorIndex = &engine;
        index
            .insert_batch(&batch_refs(&data))
            .expect("insert via trait");
        index.build_index().expect("build via trait");
        assert_eq!(index.count().expect("count"), 200);
        let results = index.search(&data[1].1, 3).expect("search via trait");
        assert_eq!(results[0].0, 1);
        index.delete_by_chunk_ids(&[1]).expect("delete via trait");
        assert_eq!(index.count().expect("count"), 199);
        let replacement = rows(10);
        index.rebuild(&replacement).expect("rebuild via trait");
        assert_eq!(index.count().expect("count"), 10);
    }
}
