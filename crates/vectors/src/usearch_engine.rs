//! USearch-backed ANN engine (add-usearch-ann-engine, task 1.2; ADR 0004
//! two-layer RAM/DISK layout, usearch-wal-persistence task 3.3).
//!
//! [`UsearchEngine`] implements the full [`crate::VectorIndex`] contract on
//! the USearch 2.26 C++11 HNSW core (cxx FFI):
//!
//! - `L2sq` metric with configurable scalar quantization (default `BF16`;
//!   [`VectorIndexConfig::quantization`] selects `u8`/`i8`/`f16`/`bf16`/`f32`):
//!   the C++ core down-casts every f32 vector itself before storage. For the
//!   integer kinds (`u8`/`i8`) each vector is normalized to unit length before
//!   the scale (`u8[i] = clamp(v[i] * 255 / ||v||, 0, 255)`), so L2sq distances
//!   on that storage behave cosine-like; the floating kinds (`bf16`/`f16`/`f32`)
//!   store near-lossless values. Only the ranking (not the absolute distance
//!   value) feeds the RRF fusion, so the quantization is a storage/latency
//!   trade-off; the Rust side always passes f32 and never works around the
//!   down-cast;
//! - HNSW parameters from [`VectorIndexConfig`]: `connectivity = m`,
//!   `expansion_add = ef_construction`, `expansion_search = ef_search`
//!   (the IVF fields `num_partitions`/`nprobes` do not apply to pure HNSW);
//! - the HNSW graph is built incrementally during every `add`, so the index
//!   is always search-ready; [`UsearchEngine::build_index`] is the
//!   persistence point (layout below).
//!
//! # On-disk layout (ADR 0004 §1)
//!
//! The engine directory (`<vectors_path>/usearch/`) holds a two-layer index:
//!
//! ```text
//! <vectors_path>/usearch/
//! ├── ram.usearch              # RAM layer snapshot (written at create, then on flush/shutdown)
//! ├── ram.keys                 # sidecar key manifest of the RAM layer
//! └── segments/
//!     ├── segment-1.usearch    # DISK[n] layer, read-only, id monotonic
//!     ├── segment-1.keys       # sidecar key manifest
//!     └── ...
//! ```
//!
//! - **RAM layer (segment id 0)** — the freshest data, kept as a mutable
//!   in-memory copy: `open` loads `ram.usearch` with
//!   `Index::restore_from_buffer` (a memory copy — the usearch core does
//!   not persist `add`/`remove` through an mmap file, ADR audit #12, so
//!   the snapshot is written explicitly by [`UsearchEngine::save`] at
//!   create time, on flush/shutdown, and by [`UsearchEngine::rebuild`];
//!   the create-time empty snapshot carries the dimensionality in the
//!   file header, so even an empty index is identifiable by dimension on
//!   `open` — the dim check always has a file to read);
//! - **DISK layers (segment id 1..N)** — historical, read-only: `open`
//!   maps each `segment-<n>.usearch` with `Index::restore_view` (mmap
//!   view, zero-copy); the ids grow monotonically and are never
//!   renumbered (ADR 0004 §2), so a higher id is always the fresher layer;
//! - **Sidecar key manifests (`.keys`)** — the usearch core exposes no
//!   key-enumeration API, so every layer's keys are recorded in a binary
//!   manifest next to the index file (the [`keys_manifest`] codec):
//!   `magic u32 LE`, `count u32 LE`, `count × key u32 LE`, written
//!   atomically (tmp + fsync + rename).
//!
//! # Startup recovery (`open`, ADR 0004 §7/§8)
//!
//! [`UsearchEngine::open_with_wal`] runs the crash-matrix recovery:
//!
//! 1. garbage cleanup — `segments.tmp/` (compaction scratch; promoted to
//!    `segments/` in the "crash between the two directory renames" case),
//!    `segments.old/`, and `*.tmp`/`*.old` files left by atomic writes are
//!    removed;
//! 2. DISK segments are mapped read-only with their sidecars (a missing or
//!    corrupt sidecar is a distinct error, never a silent gap);
//! 3. the RAM layer is restored (missing snapshot → empty index: the
//!    honest loss window, ADR 0004 §5);
//! 4. with a WAL database attached, orphan WAL rows whose segment files no
//!    longer exist are deleted (self-heal after a crashed compaction/flush)
//!    and the per-segment stale sets are loaded;
//! 5. every layer's dimensionality is checked against `config.dim`
//!    ([`VectorsError::DimensionMismatch`]).
//!
//! # WAL journal (ADR 0004 §3)
//!
//! With a SQLite database attached (a dedicated long-lived connection
//! opened by [`UsearchEngine::open_with_wal`] from the knowledge.db path,
//! or an injected one via [`UsearchEngine::with_wal_db`]), mutations are
//! journaled to the `usearch_vectors_log` table (migrations 3+4, db crate)
//! BEFORE the RAM index is mutated (WAL-first): `insert`/`insert_batch`
//! write an ADD record (flag 1), `delete_by_chunk_ids` writes a DEL record
//! (flag 2), and `rebuild` clears the table (its result is fully persisted
//! to the layout). The table is `PK (segment_id, chunk_id)`; `segment_id`
//! 0 is the RAM layer, `n` a DISK layer. The WAL stores only `chunk_id` +
//! flags — never the vector payload (design: "WAL without vectors"; the
//! payload comes from the chunks table). Without an attached database (the
//! default), every WAL call is a no-op and the engine behaves RAM-only.
//!
//! # Open engines are read-write
//!
//! The RAM layer is an in-memory copy, so an opened engine is fully
//! writable: `insert`, `insert_batch` and `delete_by_chunk_ids` work on it
//! exactly as on a created engine. Mutations land on the RAM layer until
//! persisted by a save (layout above).
//!
//! # Key enumeration
//!
//! The usearch Rust bindings expose no "list all keys" API, so
//! [`UsearchEngine::chunk_ids`] currently runs `exact_search` (a guaranteed
//! full brute-force scan) with `k = size()` and a fixed non-zero query on
//! the RAM layer: every live vector is a distinct top-k hit (the index is
//! non-multi), so the match keys are exactly the stored chunk ids. Like
//! `LanceEngine::chunk_ids` this is a full scan — the reconciliation
//! primitive of the cascade protocol (design D3), called rarely by the GC,
//! never on the query path. (Task 3.4 switches `chunk_ids`/`count` to the
//! sidecar manifests minus the stale sets.)
//!
//! # Reserve-before-mutate
//!
//! The 2.26 core rejects an insertion — and a search — that finds no
//! reserved worker thread ("Reserve capacity ahead of ..."). `create`
//! therefore reserves 1 slot (an empty index stays searchable and
//! insertable), and `insert_batch`/`rebuild` reserve the total row count up
//! front (reserve is monotonic and never shrinks). Restored indexes already
//! carry at least one worker thread from the file header, so `open` needs
//! no reserve.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use rayon::prelude::*;
use rusqlite::{Connection, params};
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

use crate::{VectorIndex, VectorIndexConfig, VectorsError};

/// RAM layer snapshot file inside the engine directory (ADR 0004 §1).
const RAM_INDEX_FILE: &str = "ram.usearch";
/// RAM layer sidecar key manifest (ADR 0004 §1/§3).
const RAM_KEYS_FILE: &str = "ram.keys";
/// DISK segment directory (ADR 0004 §1).
const SEGMENTS_DIR: &str = "segments";
/// Compaction scratch directory: the new segments are assembled here
/// before the atomic directory swap (ADR 0004 §7).
const SEGMENTS_TMP_DIR: &str = "segments.tmp";
/// Previous segment directory, kept until the compaction swap is complete
/// (ADR 0004 §7).
const SEGMENTS_OLD_DIR: &str = "segments.old";
/// DISK segment file prefix: `segment-<n>.usearch` / `segment-<n>.keys`
/// (ADR 0004 §1; `n` is decimal and monotonic).
const SEGMENT_FILE_PREFIX: &str = "segment-";
/// DISK segment index file extension.
const SEGMENT_INDEX_EXT: &str = ".usearch";
/// Rows per rayon task in `insert_batch`/`rebuild` (mirrors the
/// LanceEngine batch size of 1000; each row is a single concurrent `add`).
const ADD_CHUNK: usize = 1000;
/// WAL flag: a vector was added (usearch-wal-persistence design).
const WAL_ADD: u8 = 1;
/// WAL flag: a vector was deleted or superseded in that segment.
const WAL_DEL: u8 = 2;

/// The RAM layer snapshot file path (ADR 0004 §1).
fn ram_index_path(root: &Path) -> PathBuf {
    root.join(RAM_INDEX_FILE)
}

/// The RAM layer sidecar manifest path (ADR 0004 §1).
fn ram_keys_path(root: &Path) -> PathBuf {
    root.join(RAM_KEYS_FILE)
}

/// The DISK segment directory (ADR 0004 §1).
fn segments_dir(root: &Path) -> PathBuf {
    root.join(SEGMENTS_DIR)
}

/// The DISK segment sidecar manifest path for segment id `id`.
fn segment_keys_path(root: &Path, id: u32) -> PathBuf {
    segments_dir(root).join(format!("{SEGMENT_FILE_PREFIX}{id}.keys"))
}

/// One loaded DISK segment (ADR 0004 §1/§2): the read-only mmap view, its
/// monotonic id (the `segment-<n>` file number; higher = fresher), and the
/// sidecar key manifest (the durable key record, ADR 0004 §3).
#[derive(Clone)]
struct DiskSegment {
    id: u32,
    index: Arc<Index>,
    /// The sidecar key manifest (ADR 0004 §3): the durable key record for
    /// this layer. Task 3.4 reads it for `count`/`chunk_ids` and task 3.7
    /// for the compaction live-key set; task 3.3 only stores it.
    #[allow(dead_code)]
    keys: HashSet<u32>,
}

/// USearch-backed ANN index (ADR 0004 two-layer RAM/DISK layout).
///
/// Owns the RAM layer index (usearch `Index` is `Send + Sync`; every
/// method takes `&self` — the C++ index is concurrent by design), the
/// loaded DISK segment views, and the engine directory (ADR 0004 §1
/// layout). All methods are sync; the engine is safe to share across
/// threads and `spawn_blocking` workers.
///
/// Implements [`VectorIndex`] by pure delegation to the inherent methods,
/// so `Arc<dyn VectorIndex>` and `UsearchEngine` are interchangeable.
pub struct UsearchEngine {
    /// RAM layer (segment id 0): the freshest data, a mutable in-memory
    /// copy (ADR 0004 §1: `restore_from_buffer`, never mmap).
    index: Index,
    /// RAM key manifest: the in-memory content of `ram.keys` (ADR 0004
    /// §3). Updated on every RAM mutation; the sidecar is rewritten on
    /// save/rebuild.
    ram_keys: Mutex<HashSet<u32>>,
    /// DISK segments (segment id 1..N): read-only mmap views (ADR 0004
    /// §1). `RwLock` because rebuild/compaction replace the list while
    /// searches clone the current `Arc`s (ADR 0004 §6).
    disk_segments: RwLock<Vec<DiskSegment>>,
    config: VectorIndexConfig,
    /// The engine root directory (ADR 0004 §1 layout).
    root: PathBuf,
    /// Usearch-specific config (max_segment_vectors, compaction threshold, etc.)
    usearch_config: crate::UsearchConfig,
    /// Raw per-segment stale sets loaded from the WAL at open (ADR 0004
    /// §3/§8): `segment_id → deleted/superseded chunk ids`. Task 3.4
    /// turns this into the versioned search-path cache.
    stale: Mutex<HashMap<u32, HashSet<u32>>>,
    /// The SQLite connection holding the `usearch_vectors_log` WAL table
    /// (ADR 0004 §3); `None` (the default) disables the WAL — every WAL
    /// call is a no-op. `Mutex` because `rusqlite::Connection` is `Send`
    /// but not `Sync`, while the engine is shared across threads
    /// (`VectorIndex: Send + Sync`).
    wal: Option<Mutex<Connection>>,
}

impl UsearchEngine {
    /// Creates a new engine with an EMPTY RAM layer at `path` (creating
    /// the directory layout if needed): the empty RAM snapshot pair
    /// (`ram.usearch` + `ram.keys` — the snapshot carries the configured
    /// dimensionality, so the dimension is durable from the start) and an
    /// empty `segments/` directory (ADR 0004 §1). Fails if the layout
    /// already exists — use [`Self::open`] for that.
    pub fn create(
        path: impl Into<PathBuf>,
        config: VectorIndexConfig,
    ) -> Result<Self, VectorsError> {
        Self::create_with_config(path, config, crate::UsearchConfig::default())
    }

    /// Creates a new engine with UsearchConfig.
    pub fn create_with_config(
        path: impl Into<PathBuf>,
        config: VectorIndexConfig,
        usearch_config: crate::UsearchConfig,
    ) -> Result<Self, VectorsError> {
        let root = path.into();
        config.validate()?;
        usearch_config.validate()?;
        if layout_exists(&root) {
            return Err(VectorsError::Engine(format!(
                "index already exists at {}",
                root.display()
            )));
        }
        // ADR 0004 §1: the empty layout is durable from the start — the
        // (empty) segment directory and the RAM layer pair. The empty
        // `ram.usearch` snapshot is written at create time (not at the
        // first flush/shutdown): the usearch file header carries the
        // dimensionality, so an empty created index stays identifiable by
        // dimension on `open` — without it an 8-dim and a 4-dim empty
        // index were byte-identical on disk and the dim check never ran
        // (task 3.3 revision).
        std::fs::create_dir_all(segments_dir(&root))?;
        let index = Index::new(&options(&config)).map_err(map_usearch)?;
        // The 2.26 core rejects a search that finds no reserved worker
        // thread; reserving 1 slot keeps an empty index searchable (and
        // insertable) from the start.
        index.reserve(1).map_err(map_usearch)?;
        let engine = Self {
            index,
            ram_keys: Mutex::new(HashSet::new()),
            disk_segments: RwLock::new(Vec::new()),
            config,
            root,
            usearch_config,
            stale: Mutex::new(HashMap::new()),
            wal: None,
        };
        // The create-time snapshot: an empty `ram.usearch` (carrying the
        // dimension) + the empty `ram.keys` sidecar. `open`'s dim check
        // then always has a file to read.
        engine.save()?;
        Ok(engine)
    }

    /// Opens an engine at `path` created earlier by [`Self::create`]: the
    /// ADR 0004 §7/§8 startup recovery runs (module docs), so the opened
    /// engine is fully writable (module docs: "Open engines are
    /// read-write").
    ///
    /// Returns [`VectorsError::NotFound`] if no index layout exists at
    /// `path`, and [`VectorsError::DimensionMismatch`] if a stored
    /// layer's vector dimensionality differs from `config.dim`.
    pub fn open(path: impl Into<PathBuf>, config: VectorIndexConfig) -> Result<Self, VectorsError> {
        Self::open_with_wal(path, config, None)
    }

    /// Opens an engine at `path` with an optional WAL database (ADR 0004
    /// §8/§9): the engine opens a dedicated long-lived `rusqlite::Connection`
    /// to `wal_db` (the knowledge.db path), reconciles the orphan
    /// `usearch_vectors_log` rows (ADR 0004 §7 crash matrix), and loads
    /// the per-segment stale sets (ADR 0004 §3). `None` (the default)
    /// disables the WAL — every WAL call is a no-op.
    ///
    /// See [`Self::open`] for the error contract.
    pub fn open_with_wal(
        path: impl Into<PathBuf>,
        config: VectorIndexConfig,
        wal_db: Option<&Path>,
    ) -> Result<Self, VectorsError> {
        let root = path.into();
        config.validate()?;
        // The usearch tuning section travels with the index config
        // (ADR 0004 §10: all three fields get their real value).
        let usearch_config = config.usearch.clone().unwrap_or_default();
        if !root.is_dir() {
            return Err(VectorsError::NotFound(root.display().to_string()));
        }
        // 1. Crash garbage (ADR 0004 §7 crash matrix, §8 step 1).
        cleanup_garbage(&root)?;
        if !layout_exists(&root) {
            return Err(VectorsError::NotFound(root.display().to_string()));
        }
        // 2. DISK segments: read-only mmap views + sidecar manifests
        //    (ADR 0004 §8 step 2; the dim check is step 5).
        let disk_segments = load_disk_segments(&root, &config)?;
        // 3. RAM layer: in-memory copy of the snapshot, or empty
        //    (ADR 0004 §8 step 3).
        let (index, ram_keys) = load_ram_layer(&root, &config)?;
        // 4. WAL: dedicated connection, orphan-row reconciliation, and
        //    the raw stale sets (ADR 0004 §8 steps 4/6).
        let (wal, stale) = match wal_db {
            Some(db_path) => {
                let conn = Connection::open(db_path).map_err(map_sqlite)?;
                let disk_ids: Vec<u32> = disk_segments.iter().map(|segment| segment.id).collect();
                reconcile_wal(&conn, &disk_ids)?;
                let stale = load_stale_sets(&conn)?;
                (Some(Mutex::new(conn)), Mutex::new(stale))
            }
            None => (None, Mutex::new(HashMap::new())),
        };
        Ok(Self {
            index,
            ram_keys: Mutex::new(ram_keys),
            disk_segments: RwLock::new(disk_segments),
            config,
            root,
            usearch_config,
            stale,
            wal,
        })
    }

    /// Attaches the SQLite connection that holds the `usearch_vectors_log`
    /// WAL table (usearch-wal-persistence task 2.2) and takes ownership of
    /// it.
    ///
    /// The table is created by the knowledge database's migration
    /// `3-usearch-vectors-log` (db crate); this method does not open or
    /// migrate anything — the consumer (which owns the database) opens the
    /// connection. With the WAL attached, `insert`/`insert_batch` journal
    /// an ADD record, `delete_by_chunk_ids` journals a DEL record, and
    /// `rebuild` clears the table — each before the RAM index is mutated
    /// (WAL-first, module docs). Without it (the default from
    /// [`Self::create`]/[`Self::open`]), all WAL calls are no-ops.
    pub fn with_wal_db(mut self, conn: Connection) -> Self {
        self.wal = Some(Mutex::new(conn));
        self
    }

    /// Stores `vector` under `chunk_id` (single-row convenience over
    /// [`Self::insert_batch`]). The vector length must equal `config.dim`.
    pub fn insert(&self, chunk_id: u32, vector: &[f32]) -> Result<(), VectorsError> {
        self.insert_batch(&[(chunk_id, vector)])
    }

    /// Stores many vectors; every row is a single concurrent `add`, chunked
    /// into rayon tasks of [`ADD_CHUNK`] (the C++ index is concurrent). The
    /// total row count is reserved up front (the 2.26 core rejects
    /// insertions without reserved capacity; reserve is monotonic, so
    /// incremental ingestion never shrinks capacity).
    ///
    /// Every vector must have length `config.dim` (otherwise
    /// [`VectorsError::DimensionMismatch`], and the whole call is rejected
    /// before anything is stored). Uniqueness of `chunk_id` is the caller's
    /// responsibility.
    ///
    /// With a WAL attached ([`Self::with_wal_db`]), an ADD record is
    /// journaled for every row BEFORE the index is mutated (module docs:
    /// WAL journal).
    ///
    /// The mutation lands on the RAM layer only; persist it with
    /// [`Self::build_index`] or [`Self::rebuild`] (module docs: on-disk
    /// layout).
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
        // WAL-first: journal every row before the RAM index is mutated, so
        // a crash mid-batch leaves a self-healing log (the payload comes
        // from the chunks table on replay).
        for &(chunk_id, _) in rows {
            self.write_wal(chunk_id, WAL_ADD)?;
        }
        self.index
            .reserve(self.index.size() + rows.len())
            .map_err(map_usearch)?;
        add_rows(&self.index, rows)?;
        // The RAM key manifest tracks the index (ADR 0004 §3); the
        // sidecar is rewritten on save/rebuild.
        let mut ram_keys = mutex_guard(&self.ram_keys);
        for &(chunk_id, _) in rows {
            ram_keys.insert(chunk_id);
        }
        Ok(())
    }

    /// Top-k nearest neighbours of `query`: `(chunk_id, distance)` pairs
    /// sorted by distance ascending (L2sq on the quantized storage — see the
    /// module docs). An empty index yields an empty vec (not an error).
    /// Requires `k > 0` and `query.len() == config.dim`.
    ///
    /// WAL filtering (usearch-wal-persistence task 2.3): chunk ids whose
    /// `usearch_vectors_log` record carries the DEL or UPD bit are excluded
    /// during HNSW traversal via usearch's `filtered_search` (the filter
    /// closure is evaluated per-candidate inside the C++ core, not as a
    /// post-filter), so deleted/updated vectors never appear in the results.
    ///
    /// Per-segment WAL filtering: each segment uses cumulative WAL entries
    /// from segment 0..N, ensuring correct filtering across multiple segments.
    /// The WAL is loaded once into memory, then cumulative stale sets are
    /// computed without repeated SQL queries.
    ///
    /// The search runs across the engine's segments in parallel (rayon
    /// `par_iter`); per-segment results are merged by [`merge_results`]
    /// (dedup by chunk_id keeping the minimum distance, sort ascending,
    /// truncate to `k`).
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u32, f32)>, VectorsError> {
        self.config.validate_search(query, k)?;

        // Load all WAL stale entries once (DEL/UPD in ANY segment_id)
        let wal_by_segment = self.load_wal_grouped()?;

        // A DEL/UPD in ANY segment makes the vector stale everywhere.
        // Build a single global stale set from all segments.
        let mut global_stale: HashSet<u32> = HashSet::new();
        for ids in wal_by_segment.values() {
            global_stale.extend(ids);
        }

        // Search DISK segments with global WAL filtering
        let segments = disk_segments_read_guard(&self.disk_segments);
        let disk_results: Vec<Vec<(u32, f32)>> = segments
            .par_iter()
            .map(|segment| {
                let matches = segment
                    .index
                    .filtered_search(query, k, |key: u64| !global_stale.contains(&(key as u32)))
                    .map_err(map_usearch)?;
                let mut results = Vec::with_capacity(matches.keys.len());
                for j in 0..matches.keys.len() {
                    results.push((key_to_chunk_id(matches.keys[j])?, matches.distances[j]));
                }
                Ok(results)
            })
            .collect::<Result<Vec<_>, VectorsError>>()?;
        drop(segments);

        // Search RAM layer with global WAL filtering
        let ram_results = self
            .index
            .filtered_search(query, k, |key: u64| !global_stale.contains(&(key as u32)))
            .map_err(map_usearch)?;
        let mut ram_vec = Vec::with_capacity(ram_results.keys.len());
        for j in 0..ram_results.keys.len() {
            ram_vec.push((
                key_to_chunk_id(ram_results.keys[j])?,
                ram_results.distances[j],
            ));
        }

        // Merge all results
        let mut all_results = disk_results;
        all_results.push(ram_vec);
        Ok(merge_results(all_results, k))
    }

    /// Removes the vectors whose chunk id is in `chunk_ids`.
    ///
    /// Idempotent: ids that are not present are silently ignored (the core
    /// reports 0 removed), so calling it again with the same ids — or after
    /// a crash between the vector delete and the SQLite chunk delete
    /// (design D3) — is always safe.
    ///
    /// With a WAL attached ([`Self::with_wal_db`]), a DEL record is
    /// journaled for every id BEFORE the index is mutated (module docs:
    /// WAL journal).
    ///
    /// The mutation lands on the RAM layer only (module docs: on-disk
    /// layout): an unsaved delete leaves orphaned vectors after a restart,
    /// which the cascade protocol tolerates and the GC re-detects via
    /// `chunk_ids`/`count`.
    pub fn delete_by_chunk_ids(&self, chunk_ids: &[u32]) -> Result<(), VectorsError> {
        if chunk_ids.is_empty() {
            return Ok(());
        }
        // WAL-first: journal every id before the RAM index is mutated
        // (crash-safety as in `insert_batch`).
        for &chunk_id in chunk_ids {
            self.write_wal(chunk_id, WAL_DEL)?;
        }
        for &chunk_id in chunk_ids {
            self.index.remove(chunk_id as u64).map_err(map_usearch)?;
        }
        // The RAM key manifest tracks the index (ADR 0004 §3); the
        // sidecar is rewritten on save/rebuild.
        let mut ram_keys = mutex_guard(&self.ram_keys);
        for &chunk_id in chunk_ids {
            ram_keys.remove(&chunk_id);
        }
        Ok(())
    }

    /// All chunk ids currently stored, in no particular order.
    ///
    /// Reconciliation primitive (design D3): the GC job computes
    /// "index − SQLite → delete" from this listing. The usearch bindings
    /// expose no key-enumeration API, so this runs `exact_search` (a
    /// guaranteed full brute-force scan) with `k = size()` and a fixed
    /// non-zero query: every live vector is a distinct top-k hit (the index
    /// is non-multi), so the match keys are exactly the stored ids —
    /// soft-deleted slots are excluded by the core. Like
    /// `LanceEngine::chunk_ids` this is a full scan, called rarely by the
    /// GC, never on the query path.
    pub fn chunk_ids(&self) -> Result<Vec<u32>, VectorsError> {
        let count = self.index.size();
        if count == 0 {
            return Ok(Vec::new());
        }
        // Defensive: a restored index already carries worker threads from
        // the file header, and a created one was reserved before its first
        // add — this is a no-op in both cases.
        self.index.reserve(count).map_err(map_usearch)?;
        // A non-zero query: the integer down-cast divides by the vector norm.
        let query = vec![1.0f32; self.config.dim];
        let matches = self
            .index
            .exact_search(&query, count)
            .map_err(map_usearch)?;
        matches
            .keys
            .iter()
            .map(|&key| key_to_chunk_id(key))
            .collect()
    }

    /// Number of vectors currently stored (soft-deleted rows are not
    /// counted).
    pub fn count(&self) -> Result<u64, VectorsError> {
        Ok(self.index.size() as u64)
    }

    /// Persists the current RAM layer state (the save point of the
    /// shutdown path, ADR 0004 §4).
    ///
    /// Unlike `LanceEngine::build_index` (which builds the IvfHnswSq index
    /// over already-stored rows), the usearch HNSW graph is built
    /// incrementally during every `add` — there is nothing to (re)build;
    /// the index is search-ready the moment a row lands. This call is the
    /// engine's persistence point: it flushes the in-memory state (e.g.
    /// inserts that have not been saved yet) to disk. It never loses data.
    pub fn build_index(&self) -> Result<(), VectorsError> {
        self.save()
    }

    /// Full reset (ADR 0004 §4, the ultimate repair of the cascade
    /// protocol, design D3): replaces the entire state with `rows` and
    /// persists it:
    ///
    /// 1. the WAL is cleared entirely (every pre-rebuild record is stale);
    /// 2. every DISK segment file is deleted — the old layers must NOT
    ///    survive (ADR audit #8) — and the in-memory views are dropped;
    /// 3. the RAM layer becomes `rows` only;
    /// 4. the new state is persisted (`ram.usearch` + `ram.keys`).
    ///
    /// Every vector must have length `config.dim` (the whole call is
    /// rejected otherwise, before anything is touched). An empty `rows`
    /// slice empties the index. Rebuild from chunk text after the embedding
    /// layer re-encodes the chunks.
    ///
    /// Like `compact` in the C++ core, do not run a search concurrently
    /// with a rebuild on the same engine.
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
        // 1. The rebuild replaces the entire state and persists it, so
        //    every WAL record is stale (module docs: WAL journal).
        self.clear_wal()?;
        // 2. The old DISK layers must not survive: files first, then the
        //    in-memory views.
        let segments = segments_dir(&self.root);
        if segments.exists() {
            std::fs::remove_dir_all(&segments)?;
        }
        std::fs::create_dir_all(&segments)?;
        disk_segments_write_guard(&self.disk_segments).clear();
        // 3. RAM = rows only.
        self.index.reset().map_err(map_usearch)?;
        // Reserve the final size up front (1 slot minimum keeps an empty
        // index searchable and insertable).
        self.index.reserve(rows.len().max(1)).map_err(map_usearch)?;
        let refs: Vec<(u32, &[f32])> = rows
            .iter()
            .map(|(id, vector)| (*id, vector.as_slice()))
            .collect();
        add_rows(&self.index, &refs)?;
        let mut ram_keys = mutex_guard(&self.ram_keys);
        ram_keys.clear();
        for (id, _) in rows {
            ram_keys.insert(*id);
        }
        drop(ram_keys);
        // 4. Persist the new state (ADR 0004 §4: save ram + sidecar).
        self.save()?;
        // The WAL is empty again: the open-time stale sets are stale.
        *mutex_guard(&self.stale) = HashMap::new();
        Ok(())
    }

    /// Persists the RAM layer (ADR 0004 §1): the `ram.usearch` snapshot
    /// (atomic tmp + rename) and the `ram.keys` sidecar from the
    /// in-memory manifest. The file → sidecar order keeps a crash
    /// recoverable by [`Self::open_with_wal`] (ADR 0004 §5/§7).
    pub fn save(&self) -> Result<(), VectorsError> {
        let snapshot = ram_index_path(&self.root);
        let tmp = self.root.join(format!("{RAM_INDEX_FILE}.tmp"));
        self.index.save(&to_str(&tmp)?).map_err(map_usearch)?;
        rename_over(&tmp, &snapshot)?;
        let mut keys: Vec<u32> = mutex_guard(&self.ram_keys).iter().copied().collect();
        keys.sort_unstable();
        keys_manifest::write_keys(&ram_keys_path(&self.root), &keys)
    }

    /// The index/query configuration this engine was created with.
    pub fn config(&self) -> &VectorIndexConfig {
        &self.config
    }

    /// The engine root directory (ADR 0004 §1 layout).
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Snapshot of the raw per-segment stale sets loaded from the WAL at
    /// open (ADR 0004 §3/§8): `segment_id → deleted/superseded chunk
    /// ids`. Empty when no WAL database is attached. Task 3.4 turns this
    /// into the versioned search-path cache with write-path bumps.
    pub fn stale_sets(&self) -> HashMap<u32, HashSet<u32>> {
        mutex_guard(&self.stale).clone()
    }

    /// Journals one WAL record: `INSERT OR REPLACE` into
    /// `usearch_vectors_log` — one row per `chunk_id`, the latest
    /// operation wins, `created_at` is `datetime('now')` (UTC).
    ///
    /// A no-op when no WAL connection is attached ([`Self::with_wal_db`]).
    fn write_wal(&self, chunk_id: u32, flags: u8) -> Result<(), VectorsError> {
        self.write_wal_with_segment(chunk_id, flags, 0) // segment_id=0 = current/RAM
    }

    fn write_wal_with_segment(
        &self,
        chunk_id: u32,
        flags: u8,
        segment_id: u32,
    ) -> Result<(), VectorsError> {
        let Some(wal) = &self.wal else {
            return Ok(());
        };
        let conn = wal_guard(wal);
        conn.execute(
            "INSERT OR REPLACE INTO usearch_vectors_log (segment_id, chunk_id, flags, created_at) \
             VALUES (?1, ?2, ?3, datetime('now'))",
            params![segment_id as i64, chunk_id as i64, flags as i64],
        )
        .map_err(map_sqlite)?;
        Ok(())
    }

    /// Removes WAL records for old DISK segments (segment_id > 0) after
    /// compaction — those segments no longer exist, so all their records
    /// (ADD, DEL) are stale.
    ///
    /// segment_id = 0 records (current/RAM operations) are preserved.
    ///
    /// A no-op when no WAL connection is attached.
    fn clear_old_segments(&self) -> Result<(), VectorsError> {
        let Some(wal) = &self.wal else {
            return Ok(());
        };
        let conn = wal_guard(wal);
        conn.execute("DELETE FROM usearch_vectors_log WHERE segment_id > 0", [])
            .map_err(map_sqlite)?;
        Ok(())
    }

    /// Removes every row from `usearch_vectors_log`.
    /// Used after rebuild when the entire index is replaced.
    ///
    /// A no-op when no WAL connection is attached.
    fn clear_wal(&self) -> Result<(), VectorsError> {
        let Some(wal) = &self.wal else {
            return Ok(());
        };
        let conn = wal_guard(wal);
        conn.execute("DELETE FROM usearch_vectors_log", [])
            .map_err(map_sqlite)?;
        Ok(())
    }

    /// Check if compaction is needed and trigger it.
    pub fn maybe_compact(&mut self) -> Result<(), VectorsError> {
        let stale_pct = self.stale_vector_percentage()?;
        if stale_pct > self.usearch_config.compaction_stale_threshold as f64 {
            self.compact()?;
        }
        Ok(())
    }

    /// Merge all DISK segments, remove stale vectors, create new segments.
    /// RAM index is NOT touched — it's the current working set.
    /// If there are no DISK segments, this is a no-op.
    ///
    /// Uses SQL query to enumerate live keys per segment instead of
    /// `exact_search` (O(N) brute-force with distance computation).
    ///
    /// Transitional (task 3.3): the merge result replaces the in-memory
    /// list only — the on-disk directory swap (ADR 0004 §7) lands in task
    /// 3.7. New segments take the next monotonic ids (ADR 0004 §2).
    fn compact(&mut self) -> Result<(), VectorsError> {
        // No DISK segments to compact.
        {
            let segments = disk_segments_read_guard(&self.disk_segments);
            if segments.is_empty() {
                return Ok(());
            }
        }

        // 1. Load live keys per DISK segment via SQL (O(log N) indexed
        //    query).
        let live_keys_per_seg = self.load_live_keys_per_segment()?;

        // 2. Collect live vectors from DISK segments using get().
        //    Deduplicate by key: later segments win (higher segment_id =
        //    more recent snapshot).
        let dim = self.config.dim;
        let mut seen: HashSet<u32> = HashSet::new();
        let mut live_vectors: Vec<(u32, Vec<f32>)> = Vec::new();
        {
            let segments = disk_segments_read_guard(&self.disk_segments);
            for segment in segments.iter() {
                let keys = live_keys_per_seg.get(&segment.id);
                let Some(keys) = keys else {
                    continue;
                };
                for id in keys {
                    // Skip duplicates: later segment version wins.
                    if !seen.insert(*id) {
                        continue;
                    }
                    let mut vector = vec![0.0f32; dim];
                    segment
                        .index
                        .get::<f32>(*id as u64, &mut vector)
                        .map_err(map_usearch)?;
                    live_vectors.push((*id, vector));
                }
            }
        }

        // 3. Create new segments (sliced by max_segment_vectors) with the
        //    next monotonic ids.
        let next_id = {
            let segments = disk_segments_read_guard(&self.disk_segments);
            segments.iter().map(|segment| segment.id).max().unwrap_or(0) + 1
        };
        let max = self.usearch_config.max_segment_vectors;
        let mut new_segments = Vec::new();
        for (segment_id, chunk) in (next_id..).zip(live_vectors.chunks(max)) {
            let new_index = Index::new(&options(&self.config)).map_err(map_usearch)?;
            new_index.reserve(chunk.len().max(1)).map_err(map_usearch)?;
            for (id, vector) in chunk {
                new_index.add(*id as u64, vector).map_err(map_usearch)?;
            }
            let keys: HashSet<u32> = chunk.iter().map(|(id, _)| *id).collect();
            new_segments.push(DiskSegment {
                id: segment_id,
                index: Arc::new(new_index),
                keys,
            });
        }

        // 4. Replace old segments (in-memory list; on-disk swap is 3.7).
        disk_segments_write_guard(&self.disk_segments).clone_from(&new_segments);

        // 5. Remove old DISK segment records from WAL (RAM records remain)
        self.clear_old_segments()?;

        Ok(())
    }

    /// Load live keys per DISK segment from WAL.
    /// Returns a map: segment_id → Vec<chunk_id> (live keys only).
    ///
    /// A key is live if it has an ADD record with `segment_id > 0` (DISK)
    /// and no DEL/UPD record for that chunk_id (in ANY segment_id).
    /// This handles the cross-segment stale problem: `delete_by_chunk_ids`
    /// writes DEL with `segment_id=0`, but the ADD records are in the
    /// DISK segment's `segment_id`.
    fn load_live_keys_per_segment(&self) -> Result<HashMap<u32, Vec<u32>>, VectorsError> {
        let Some(wal) = &self.wal else {
            return Ok(HashMap::new());
        };
        let conn = wal_guard(wal);

        // A key is live in a DISK segment if:
        // 1. It has an ADD record in that segment (flags = ADD, segment_id > 0)
        // 2. No DEL/UPD record exists for that chunk_id in ANY segment
        let mut stmt = conn
            .prepare(
                "SELECT segment_id, chunk_id FROM usearch_vectors_log \
                 WHERE flags = ?1 AND segment_id > 0 \
                 AND chunk_id NOT IN \
                   (SELECT chunk_id FROM usearch_vectors_log \
                    WHERE flags = ?2)",
            )
            .map_err(map_sqlite)?;
        let rows = stmt
            .query_map(params![WAL_ADD as i64, WAL_DEL as i64], |row| {
                let segment_id = row.get::<_, i64>(0)?;
                let chunk_id = row.get::<_, i64>(1)?;
                Ok((segment_id, chunk_id))
            })
            .map_err(map_sqlite)?;

        let mut result: HashMap<u32, Vec<u32>> = HashMap::new();
        for row in rows {
            let (segment_id, chunk_id) = row.map_err(map_sqlite)?;
            let segment_id = u32::try_from(segment_id).map_err(|_| {
                VectorsError::Engine(format!("WAL segment_id {segment_id} exceeds u32 range"))
            })?;
            let chunk_id = u32::try_from(chunk_id).map_err(|_| {
                VectorsError::Engine(format!("WAL chunk_id {chunk_id} exceeds u32 range"))
            })?;
            result.entry(segment_id).or_default().push(chunk_id);
        }
        Ok(result)
    }

    /// Calculate percentage of stale vectors in DISK segments.
    /// Returns 0.0 if there are no DISK segments.
    ///
    /// Only counts DEL/UPD records where the chunk_id has a corresponding
    /// ADD record in a DISK segment (segment_id > 0). This avoids counting
    /// orphaned DEL records from segment_id=0 that don't correspond to
    /// any DISK vectors.
    fn stale_vector_percentage(&self) -> Result<f64, VectorsError> {
        let total = {
            let segments = disk_segments_read_guard(&self.disk_segments);
            segments
                .iter()
                .map(|segment| segment.index.size())
                .sum::<usize>()
        };
        if total == 0 {
            return Ok(0.0);
        }
        let stale = self.count_stale_in_disk()?;
        Ok(stale as f64 / total as f64 * 100.0)
    }

    /// Count stale vectors that exist in DISK segments.
    /// A vector is stale if it has a DEL record AND an ADD record
    /// with segment_id > 0 (i.e., it's in a DISK segment).
    fn count_stale_in_disk(&self) -> Result<usize, VectorsError> {
        let Some(wal) = &self.wal else {
            return Ok(0);
        };
        let conn = wal_guard(wal);
        let mut stmt = conn
            .prepare(
                "SELECT COUNT(DISTINCT chunk_id) FROM usearch_vectors_log \
                 WHERE flags = ?1 \
                 AND chunk_id IN \
                   (SELECT chunk_id FROM usearch_vectors_log \
                    WHERE flags = ?2 AND segment_id > 0)",
            )
            .map_err(map_sqlite)?;
        let count: i64 = stmt
            .query_row(params![WAL_DEL as i64, WAL_ADD as i64], |row| row.get(0))
            .map_err(map_sqlite)?;
        Ok(count as usize)
    }

    /// Load all WAL entries with DEL flag, grouped by segment_id.
    /// Returns a map: segment_id → HashSet<chunk_id>.
    ///
    /// Only DEL entries are included — UPD is not used in the current
    /// implementation and should not make vectors stale (UPD means
    /// "updated", i.e. old version replaced, new version is live).
    fn load_wal_grouped(&self) -> Result<HashMap<u32, HashSet<u32>>, VectorsError> {
        let Some(wal) = &self.wal else {
            return Ok(HashMap::new());
        };
        load_stale_sets(&wal_guard(wal))
    }
}

/// Pure delegation to the inherent methods, which carry the full
/// documentation. Fully-qualified `UsearchEngine::method` calls make the
/// delegation unambiguous (inherent methods shadow trait methods, but the
/// explicit form keeps the intent readable).
impl VectorIndex for UsearchEngine {
    fn insert(&self, chunk_id: u32, vector: &[f32]) -> Result<(), VectorsError> {
        UsearchEngine::insert(self, chunk_id, vector)
    }

    fn insert_batch(&self, rows: &[(u32, &[f32])]) -> Result<(), VectorsError> {
        UsearchEngine::insert_batch(self, rows)
    }

    fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u32, f32)>, VectorsError> {
        UsearchEngine::search(self, query, k)
    }

    fn delete_by_chunk_ids(&self, chunk_ids: &[u32]) -> Result<(), VectorsError> {
        UsearchEngine::delete_by_chunk_ids(self, chunk_ids)
    }

    fn chunk_ids(&self) -> Result<Vec<u32>, VectorsError> {
        UsearchEngine::chunk_ids(self)
    }

    fn count(&self) -> Result<u64, VectorsError> {
        UsearchEngine::count(self)
    }

    fn build_index(&self) -> Result<(), VectorsError> {
        UsearchEngine::build_index(self)
    }

    fn rebuild(&self, rows: &[(u32, Vec<f32>)]) -> Result<(), VectorsError> {
        UsearchEngine::rebuild(self, rows)
    }
}

/// Adds all rows to a concurrent index, chunked into rayon tasks of
/// [`ADD_CHUNK`] (the rows must already be dim-validated by the caller).
fn add_rows(index: &Index, rows: &[(u32, &[f32])]) -> Result<(), VectorsError> {
    rows.par_chunks(ADD_CHUNK)
        .try_for_each(|chunk| {
            for &(chunk_id, vector) in chunk {
                index.add(chunk_id as u64, vector).map_err(map_usearch)?;
            }
            Ok(())
        })
        .map(|_| ())
}

/// Merges per-segment search results (usearch-wal-persistence task 2.3):
/// deduplicates by `chunk_id` (keeping the minimum distance, since each
/// chunk should live in exactly one segment — a duplicate indicates a
/// transition state), sorts by distance ascending (id tiebreak), and
/// truncates to `k`.
fn merge_results(results: Vec<Vec<(u32, f32)>>, k: usize) -> Vec<(u32, f32)> {
    use std::collections::HashMap;

    let mut merged: HashMap<u32, f32> = HashMap::new();
    for segment_results in results {
        for (id, distance) in segment_results {
            merged
                .entry(id)
                .and_modify(|existing| *existing = existing.min(distance))
                .or_insert(distance);
        }
    }
    let mut sorted: Vec<_> = merged.into_iter().collect();
    sorted.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    sorted.truncate(k);
    sorted
}

/// Sidecar key manifest codec (usearch-wal-persistence task 3.2, ADR 0004 §3).
///
/// The manifest is the durable record of every key stored in a layer (the
/// usearch core exposes no key-enumeration API, so the manifest is the only
/// key listing). The engine's on-disk layout (task 3.3) writes the manifest
/// next to every index file and reads it back on `create`/`open`/`save`.
mod keys_manifest {
    use std::path::Path;

    use crate::VectorsError;

    /// Magic for the sidecar key manifest: the ASCII bytes `"SKEY"` as a
    /// little-endian u32 (ADR 0004 §3: `magic u32 LE = 0x534B4559`).
    const KEYS_MAGIC: u32 = 0x53_4B_45_59;
    /// Manifest header size in bytes: magic u32 + count u32.
    const KEYS_HEADER_LEN: usize = 8;
    /// Size of one key record in bytes.
    const KEYS_KEY_LEN: usize = 4;

    /// Reads one little-endian u32 at `offset` from `bytes`.
    ///
    /// The caller must guarantee `offset + 4 <= bytes.len()`: [`read_keys`]
    /// validates the total file length before any record is read.
    fn keys_u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ])
    }

    /// Serializes `keys` to the sidecar key manifest format (ADR 0004 §3)
    /// and writes it to `path` atomically: the payload is written to a
    /// sibling `<stem>.tmp` file, fsynced, and renamed over `path` — atomic
    /// on the same filesystem, so a crash mid-write leaves the previous
    /// manifest intact and only a temp file, which startup garbage cleanup
    /// removes (ADR 0004 §8).
    ///
    /// Format: `magic u32 LE` ([`KEYS_MAGIC`]), `count u32 LE`, then
    /// `count ×` key `u32 LE`.
    pub(super) fn write_keys(path: &Path, keys: &[u32]) -> Result<(), VectorsError> {
        let count = u32::try_from(keys.len()).map_err(|_| {
            VectorsError::InvalidArgument(format!(
                "key manifest holds {} keys, more than the u32 count field can name",
                keys.len()
            ))
        })?;
        let mut bytes = Vec::with_capacity(KEYS_HEADER_LEN + keys.len() * KEYS_KEY_LEN);
        bytes.extend_from_slice(&KEYS_MAGIC.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
        for &key in keys {
            bytes.extend_from_slice(&key.to_le_bytes());
        }
        let tmp = path.with_extension("tmp");
        let mut file = std::fs::File::create(&tmp)?;
        let completed = std::io::Write::write_all(&mut file, &bytes)
            .and_then(|()| file.sync_data())
            .and_then(|()| std::fs::rename(&tmp, path).map(drop));
        if let Err(io_err) = completed {
            // A failed write leaves no half-manifest: the temp file is the
            // only artifact and it is removed here (startup cleanup would
            // pick it up regardless — ADR 0004 §8).
            let _ = std::fs::remove_file(&tmp);
            return Err(VectorsError::Io(io_err));
        }
        Ok(())
    }

    /// Reads and validates the sidecar key manifest at `path` (the inverse
    /// of [`write_keys`]).
    ///
    /// Returns the manifest's keys in file order. Failures are distinct and
    /// never panic:
    ///
    /// - missing file → [`VectorsError::NotFound`] (the payload is `path`);
    /// - a file shorter than the 8-byte header, or fewer key bytes than the
    ///   declared count → [`VectorsError::KeysTruncated`];
    /// - a magic other than [`KEYS_MAGIC`] → [`VectorsError::KeysBadMagic`];
    /// - trailing bytes after the declared key count →
    ///   [`VectorsError::KeysTrailingBytes`].
    pub(super) fn read_keys(path: &Path) -> Result<Vec<u32>, VectorsError> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(VectorsError::NotFound(path.display().to_string()));
            }
            Err(err) => return Err(VectorsError::Io(err)),
        };
        if bytes.len() < KEYS_HEADER_LEN {
            return Err(VectorsError::KeysTruncated(format!(
                "{}: header needs {} bytes, file is {} bytes",
                path.display(),
                KEYS_HEADER_LEN,
                bytes.len()
            )));
        }
        if keys_u32_at(&bytes, 0) != KEYS_MAGIC {
            return Err(VectorsError::KeysBadMagic);
        }
        let count = keys_u32_at(&bytes, 4);
        // Checked arithmetic: a corrupt count field must not overflow the
        // length computation.
        let expected_len = usize::try_from(count)
            .ok()
            .and_then(|c| {
                c.checked_mul(KEYS_KEY_LEN)
                    .and_then(|n| n.checked_add(KEYS_HEADER_LEN))
            })
            .ok_or_else(|| {
                VectorsError::KeysTruncated(format!(
                    "{}: declared key count {} overflows the addressable file size",
                    path.display(),
                    count
                ))
            })?;
        if bytes.len() < expected_len {
            return Err(VectorsError::KeysTruncated(format!(
                "{}: declares {} keys ({} payload bytes) but the file is only {} bytes",
                path.display(),
                count,
                expected_len - KEYS_HEADER_LEN,
                bytes.len()
            )));
        }
        if bytes.len() > expected_len {
            return Err(VectorsError::KeysTrailingBytes(
                bytes.len() - expected_len,
                count,
            ));
        }
        let mut keys = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            keys.push(keys_u32_at(&bytes, KEYS_HEADER_LEN + i * KEYS_KEY_LEN));
        }
        Ok(keys)
    }
}

/// Index options for the configured geometry: `L2sq` metric, the configured
/// scalar quantization (default `BF16`; see
/// [`VectorIndexConfig::quantization`]), HNSW `connectivity = m`,
/// `expansion_add = ef_construction`, `expansion_search = ef_search` (design.md
/// parity parameters; the IVF fields of [`VectorIndexConfig`] do not apply to
/// pure HNSW). `multi` is off: one vector per chunk id, so search results
/// never contain duplicate keys.
fn options(config: &VectorIndexConfig) -> IndexOptions {
    IndexOptions {
        dimensions: config.dim,
        metric: MetricKind::L2sq,
        quantization: quantization(config.quantization.as_deref()),
        connectivity: config.m,
        expansion_add: config.ef_construction,
        expansion_search: config.ef_search,
        multi: false,
    }
}

/// Maps the configured quantization string to a usearch [`ScalarKind`].
/// `None` (absent) and `"bf16"` resolve to `BF16` (the engine default); an
/// unrecognized value falls back to `BF16` too — the config crate rejects
/// unknown values at parse time, so the fallback is a defense-in-depth guard.
fn quantization(kind: Option<&str>) -> ScalarKind {
    match kind.map(|k| k.to_ascii_lowercase()).as_deref() {
        Some("u8") => ScalarKind::U8,
        Some("i8") => ScalarKind::I8,
        Some("f16") => ScalarKind::F16,
        Some("f32") => ScalarKind::F32,
        // "bf16", absent (None), and unrecognized values → the BF16 default.
        _ => ScalarKind::BF16,
    }
}

/// The trait keys are `u32` chunk ids; usearch keys are `u64`. Every key in
/// this engine was inserted as a `u32`, so the conversion cannot fail in
/// practice — a failure would mean a corrupted index.
fn key_to_chunk_id(key: u64) -> Result<u32, VectorsError> {
    u32::try_from(key).map_err(|_| {
        VectorsError::Engine(format!("usearch key {key} exceeds the u32 chunk-id range"))
    })
}

/// Maps a usearch cxx FFI exception to [`VectorsError::Engine`].
fn map_usearch(err: cxx::Exception) -> VectorsError {
    VectorsError::Engine(format!("usearch: {err}"))
}

/// Locks the WAL connection, recovering the guard from a poisoned mutex:
/// a panic in an earlier WAL operation does not make the connection
/// unusable (SQLite rolls a panicked statement back itself, so the
/// database state stays consistent).
fn wal_guard(wal: &Mutex<Connection>) -> MutexGuard<'_, Connection> {
    match wal.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Maps a rusqlite failure (WAL table access) to [`VectorsError::Engine`].
fn map_sqlite(err: rusqlite::Error) -> VectorsError {
    VectorsError::Engine(format!("usearch WAL: {err}"))
}

/// usearch FFI paths are C strings: the path must be valid UTF-8.
fn to_str(path: &Path) -> Result<String, VectorsError> {
    path.to_str().map(str::to_string).ok_or_else(|| {
        VectorsError::InvalidArgument(format!("path is not valid UTF-8: {}", path.display()))
    })
}

/// Locks a `Mutex`, recovering the guard from a poisoned mutex: a panic in
/// an earlier operation does not make the locked state unusable (the
/// engine's invariants are restored by the next save/rebuild).
fn mutex_guard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Locks the DISK segment list for reading (ADR 0004 §6 step 2: searches
/// clone the current `Arc`s while a rebuild/compaction replaces the list).
fn disk_segments_read_guard(
    segments: &RwLock<Vec<DiskSegment>>,
) -> RwLockReadGuard<'_, Vec<DiskSegment>> {
    match segments.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Locks the DISK segment list for writing (rebuild/compaction swap).
fn disk_segments_write_guard(
    segments: &RwLock<Vec<DiskSegment>>,
) -> RwLockWriteGuard<'_, Vec<DiskSegment>> {
    match segments.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Atomic replace of `target` by `tmp`: `rename` over an existing target is
/// atomic on Unix but fails on Windows; the fallback (remove + rename)
/// opens a tiny non-atomic window, bounded by the rebuild-repair of the
/// cascade protocol.
fn rename_over(tmp: &Path, target: &Path) -> Result<(), VectorsError> {
    if std::fs::rename(tmp, target).is_err() {
        std::fs::remove_file(target)?;
        std::fs::rename(tmp, target)?;
    }
    Ok(())
}

/// True if the ADR 0004 §1 layout exists at `root`: a `ram.keys` manifest,
/// a `ram.usearch` snapshot, or a `segments/` directory.
fn layout_exists(root: &Path) -> bool {
    ram_keys_path(root).is_file() || ram_index_path(root).is_file() || segments_dir(root).is_dir()
}

/// A fresh empty RAM-layer index (the create-state of ADR 0004 §8 step 3).
fn empty_index(config: &VectorIndexConfig) -> Result<Index, VectorsError> {
    let index = Index::new(&options(config)).map_err(map_usearch)?;
    // The 2.26 core rejects a search that finds no reserved worker thread;
    // reserving 1 slot keeps an empty index searchable and insertable.
    index.reserve(1).map_err(map_usearch)?;
    Ok(index)
}

/// Removes crash garbage (ADR 0004 §7 crash matrix, §8 step 1): the
/// `segments.tmp/` and `segments.old/` directories and `*.tmp`/`*.old`
/// files left by atomic writes. The special case "no `segments/` but a
/// `segments.tmp/`" (a crash between the two directory renames of a
/// compaction) is repaired by promoting the scratch directory to the live
/// one.
fn cleanup_garbage(root: &Path) -> Result<(), VectorsError> {
    let tmp_dir = root.join(SEGMENTS_TMP_DIR);
    let live_dir = segments_dir(root);
    let old_dir = root.join(SEGMENTS_OLD_DIR);
    if tmp_dir.is_dir() {
        if live_dir.is_dir() {
            std::fs::remove_dir_all(&tmp_dir)?;
        } else {
            // Crash between the two renames: the scratch directory holds
            // the compaction result and the live directory is gone.
            std::fs::rename(&tmp_dir, &live_dir)?;
        }
    }
    if old_dir.is_dir() {
        std::fs::remove_dir_all(&old_dir)?;
    }
    // Atomic-write residue in the root and the live segment directory.
    for dir in [root, live_dir.as_path()] {
        if !dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            let is_garbage = name.ends_with(".tmp") || name.ends_with(".old");
            if is_garbage && entry.file_type()?.is_file() {
                std::fs::remove_file(entry.path())?;
            }
        }
    }
    Ok(())
}

/// Loads the DISK segments (ADR 0004 §8 step 2): scans `segments/` for
/// `segment-<n>.usearch` files, maps each read-only with its sidecar key
/// manifest, and checks the dimensionality (step 5). A missing or corrupt
/// sidecar is a distinct error, never a silent gap (ADR 0004 §3).
fn load_disk_segments(
    root: &Path,
    config: &VectorIndexConfig,
) -> Result<Vec<DiskSegment>, VectorsError> {
    let dir = segments_dir(root);
    let mut segments = Vec::new();
    if dir.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    return false;
                };
                name.starts_with(SEGMENT_FILE_PREFIX) && name.ends_with(SEGMENT_INDEX_EXT)
            })
            .collect();
        files.sort();
        for path in files {
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            let id: u32 = file_name
                .strip_prefix(SEGMENT_FILE_PREFIX)
                .and_then(|rest| rest.strip_suffix(SEGMENT_INDEX_EXT))
                .and_then(|id_text| id_text.parse().ok())
                .ok_or_else(|| {
                    VectorsError::Engine(format!("segment file has a non-numeric id: {file_name}"))
                })?;
            // The sidecar is the durable key record (ADR 0004 §3); a
            // missing or corrupt manifest is a distinct error.
            let keys = keys_manifest::read_keys(&segment_keys_path(root, id))?;
            let index = Index::restore_view(&to_str(&path)?).map_err(map_usearch)?;
            if index.dimensions() != config.dim {
                return Err(VectorsError::DimensionMismatch {
                    expected: config.dim,
                    actual: index.dimensions(),
                });
            }
            segments.push(DiskSegment {
                id,
                index: Arc::new(index),
                keys: keys.into_iter().collect(),
            });
        }
    }
    segments.sort_by_key(|segment| segment.id);
    Ok(segments)
}

/// Loads the RAM layer (ADR 0004 §8 step 3): the `ram.usearch` snapshot
/// into an in-memory copy (`restore_from_buffer` — never mmap, ADR audit
/// #12), or an empty index if the snapshot is absent (the honest loss
/// window, ADR 0004 §5). The key manifest comes from `ram.keys`; a
/// snapshot without its manifest is a distinct error (ADR 0004 §3).
fn load_ram_layer(
    root: &Path,
    config: &VectorIndexConfig,
) -> Result<(Index, HashSet<u32>), VectorsError> {
    let snapshot = ram_index_path(root);
    let keys_path = ram_keys_path(root);
    if snapshot.is_file() {
        let bytes = std::fs::read(&snapshot)?;
        let index = Index::restore_from_buffer(&bytes).map_err(map_usearch)?;
        if index.dimensions() != config.dim {
            return Err(VectorsError::DimensionMismatch {
                expected: config.dim,
                actual: index.dimensions(),
            });
        }
        let keys = keys_manifest::read_keys(&keys_path)?;
        return Ok((index, keys.into_iter().collect()));
    }
    // No snapshot: the RAM layer is empty (create-state or the loss
    // window). The manifest, when present, is trusted for the key listing.
    if keys_path.is_file() {
        let keys = keys_manifest::read_keys(&keys_path)?;
        return Ok((empty_index(config)?, keys.into_iter().collect()));
    }
    Ok((empty_index(config)?, HashSet::new()))
}

/// WAL reconciliation on open (ADR 0004 §8 step 4): removes the orphan
/// rows whose segment files no longer exist — the self-heal after a
/// crashed compaction/flush. `segment_id = 0` (the RAM layer) is always
/// kept.
fn reconcile_wal(conn: &Connection, disk_ids: &[u32]) -> Result<(), VectorsError> {
    let mut stmt = conn
        .prepare("SELECT DISTINCT segment_id FROM usearch_vectors_log WHERE segment_id != 0")
        .map_err(map_sqlite)?;
    let ids: Vec<i64> = stmt
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(map_sqlite)?
        .collect::<Result<_, _>>()
        .map_err(map_sqlite)?;
    for id in ids {
        let exists = disk_ids.contains(&(u32::try_from(id).unwrap_or(u32::MAX)));
        if !exists {
            conn.execute(
                "DELETE FROM usearch_vectors_log WHERE segment_id = ?1",
                params![id],
            )
            .map_err(map_sqlite)?;
        }
    }
    Ok(())
}

/// Loads the raw per-segment stale sets from the WAL (ADR 0004 §8 step 6):
/// `segment_id → deleted/superseded chunk ids` for every DEL record. The
/// raw map is what task 3.4 turns into the versioned search-path cache.
fn load_stale_sets(conn: &Connection) -> Result<HashMap<u32, HashSet<u32>>, VectorsError> {
    let mut stmt = conn
        .prepare("SELECT segment_id, chunk_id FROM usearch_vectors_log WHERE flags = ?1")
        .map_err(map_sqlite)?;
    let rows = stmt
        .query_map(params![WAL_DEL as i64], |row| {
            let segment_id = row.get::<_, i64>(0)?;
            let chunk_id = row.get::<_, i64>(1)?;
            Ok((segment_id, chunk_id))
        })
        .map_err(map_sqlite)?;
    let mut stale: HashMap<u32, HashSet<u32>> = HashMap::new();
    for row in rows {
        let (segment_id, chunk_id) = row.map_err(map_sqlite)?;
        let segment_id = u32::try_from(segment_id).map_err(|_| {
            VectorsError::Engine(format!("WAL segment_id {segment_id} exceeds u32 range"))
        })?;
        let chunk_id = u32::try_from(chunk_id).map_err(|_| {
            VectorsError::Engine(format!("WAL chunk_id {chunk_id} exceeds u32 range"))
        })?;
        stale.entry(segment_id).or_default().insert(chunk_id);
    }
    Ok(stale)
}

#[cfg(test)]
mod test_util {
    //! Shared test fixtures: a unique temporary directory that removes
    //! itself (and its contents) when dropped.

    // Test code: unwrap/expect are intentional (the fixture is deterministic).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique temporary directory that removes itself (and its contents)
    /// when dropped.
    pub(crate) struct TempDir(pub(crate) PathBuf);

    impl TempDir {
        pub(crate) fn new(tag: &str) -> Self {
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
}

#[cfg(test)]
mod keys_tests {
    //! Sidecar key manifest codec tests (usearch-wal-persistence task 3.2).

    // Test code: unwrap/expect are intentional (the fixtures are deterministic).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::keys_manifest::{read_keys, write_keys};
    use super::test_util::TempDir;
    use super::*;

    /// The on-disk spelling of the `"SKEY"` magic (0x534B4559, little-endian).
    const MAGIC_BYTES: [u8; 4] = [0x59, 0x45, 0x4B, 0x53];

    #[test]
    fn round_trip_zero_keys() {
        let dir = TempDir::new("zero");
        let path = dir.0.join("segment-1.keys");
        write_keys(&path, &[]).unwrap();
        assert_eq!(read_keys(&path).unwrap(), Vec::<u32>::new());
        // Exact byte layout of the empty manifest: magic LE + zero count.
        assert_eq!(
            std::fs::read(&path).unwrap(),
            [
                MAGIC_BYTES[0],
                MAGIC_BYTES[1],
                MAGIC_BYTES[2],
                MAGIC_BYTES[3],
                0,
                0,
                0,
                0
            ]
        );
    }

    #[test]
    fn round_trip_single_key() {
        let dir = TempDir::new("single");
        let path = dir.0.join("ram.keys");
        write_keys(&path, &[42]).unwrap();
        assert_eq!(read_keys(&path).unwrap(), vec![42]);
        // Exact byte layout: magic LE + count 1 LE + key 42 LE.
        assert_eq!(
            std::fs::read(&path).unwrap(),
            [
                MAGIC_BYTES[0],
                MAGIC_BYTES[1],
                MAGIC_BYTES[2],
                MAGIC_BYTES[3],
                1,
                0,
                0,
                0,
                42,
                0,
                0,
                0
            ]
        );
    }

    #[test]
    fn round_trip_many_keys() {
        let dir = TempDir::new("many");
        let path = dir.0.join("segment-2.keys");
        let keys: Vec<u32> = (0..1000).rev().collect();
        write_keys(&path, &keys).unwrap();
        assert_eq!(read_keys(&path).unwrap(), keys);
    }

    #[test]
    fn read_missing_file_is_not_found() {
        let dir = TempDir::new("missing");
        let path = dir.0.join("nope.keys");
        match read_keys(&path) {
            Err(VectorsError::NotFound(payload)) => {
                assert!(
                    payload.contains("nope.keys"),
                    "the error must name the path: {payload}"
                );
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn read_bad_magic_is_distinct_error() {
        let dir = TempDir::new("bad-magic");
        let path = dir.0.join("bad.keys");
        let mut bytes = [0u8; 8];
        bytes[0..4].copy_from_slice(&MAGIC_BYTES);
        bytes[0] = 0x00; // corrupt the magic
        std::fs::write(&path, bytes).unwrap();
        assert!(
            matches!(read_keys(&path), Err(VectorsError::KeysBadMagic)),
            "a corrupted magic must be a distinct KeysBadMagic error, not a panic"
        );
    }

    #[test]
    fn read_truncated_payload_is_distinct_error() {
        let dir = TempDir::new("truncated");
        // The header declares 3 keys but only 2 key records are present.
        let path = dir.0.join("short.keys");
        std::fs::write(
            &path,
            [
                MAGIC_BYTES[0],
                MAGIC_BYTES[1],
                MAGIC_BYTES[2],
                MAGIC_BYTES[3],
                3,
                0,
                0,
                0,
                1,
                0,
                0,
                0,
                2,
                0,
                0,
                0,
            ],
        )
        .unwrap();
        assert!(
            matches!(read_keys(&path), Err(VectorsError::KeysTruncated(_))),
            "a short payload must be a distinct KeysTruncated error, not a panic"
        );
        // A file shorter than the 8-byte header is truncated as well.
        let headerless = dir.0.join("headerless.keys");
        std::fs::write(&headerless, [MAGIC_BYTES[0], MAGIC_BYTES[1]]).unwrap();
        assert!(matches!(
            read_keys(&headerless),
            Err(VectorsError::KeysTruncated(_))
        ));
    }

    #[test]
    fn read_trailing_bytes_is_distinct_error() {
        let dir = TempDir::new("trailing");
        let path = dir.0.join("extra.keys");
        let mut bytes = vec![
            MAGIC_BYTES[0],
            MAGIC_BYTES[1],
            MAGIC_BYTES[2],
            MAGIC_BYTES[3],
            1,
            0,
            0,
            0,
            7,
            0,
            0,
            0,
        ];
        bytes.push(0xFF); // one byte beyond the declared count
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            matches!(read_keys(&path), Err(VectorsError::KeysTrailingBytes(1, 1))),
            "trailing bytes must be a distinct KeysTrailingBytes error, not a panic"
        );
    }

    #[test]
    fn write_leaves_no_tmp_residue() {
        let dir = TempDir::new("no-residue");
        let path = dir.0.join("segment-3.keys");
        write_keys(&path, &[1, 2, 3]).unwrap();
        let entries: Vec<String> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            vec!["segment-3.keys".to_string()],
            "a successful write must leave only the manifest itself: {entries:?}"
        );
    }

    #[test]
    fn rewrite_is_atomic_replace() {
        // A second write over an existing manifest replaces it cleanly (the
        // rename target already exists) and the content converges.
        let dir = TempDir::new("rewrite");
        let path = dir.0.join("ram.keys");
        write_keys(&path, &[1, 2, 3]).unwrap();
        write_keys(&path, &[9]).unwrap();
        assert_eq!(read_keys(&path).unwrap(), vec![9]);
    }
}

#[cfg(test)]
mod layout_tests {
    //! On-disk layout, startup recovery, and rebuild tests
    //! (usearch-wal-persistence task 3.3, ADR 0004 §1/§4/§7/§8).

    // Test code: unwrap/expect are intentional (the fixtures are deterministic).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::Path;

    use rusqlite::Connection;

    use super::keys_manifest::{read_keys, write_keys};
    use super::test_util::TempDir;
    use super::*;

    /// A small, fast test config (dim 8, minimal HNSW parameters).
    fn test_config() -> VectorIndexConfig {
        VectorIndexConfig::new(8, 4, 8, 1, 1, 8).expect("valid test config")
    }

    /// A deterministic test vector: unit vector on axis `axis`.
    fn test_vector(dim: usize, axis: usize) -> Vec<f32> {
        let mut vector = vec![0.0f32; dim];
        vector[axis % dim] = 1.0;
        vector
    }

    /// Creates the ADR 0004 WAL table (migrations 3+4 shape) in `conn`.
    fn create_wal_table(conn: &Connection) {
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
        .unwrap();
    }

    /// Inserts one WAL row (flags: 1 = ADD, 2 = DEL).
    fn insert_wal_row(conn: &Connection, segment_id: u32, chunk_id: u32, flags: u8) {
        conn.execute(
            "INSERT OR REPLACE INTO usearch_vectors_log (segment_id, chunk_id, flags, created_at)
             VALUES (?1, ?2, ?3, datetime('now'))",
            params![segment_id as i64, chunk_id as i64, flags as i64],
        )
        .unwrap();
    }

    /// The WAL rows as `(segment_id, chunk_id)` pairs, ordered.
    fn wal_rows(conn: &Connection) -> Vec<(i64, i64)> {
        let mut stmt = conn
            .prepare(
                "SELECT segment_id, chunk_id FROM usearch_vectors_log \
                 ORDER BY segment_id, chunk_id",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
            .unwrap();
        rows.collect::<Result<_, _>>().unwrap()
    }

    /// Writes one DISK segment file pair (index + sidecar manifest) with the
    /// given keys, each key stored as a distinct unit vector.
    fn write_segment(root: &Path, id: u32, config: &VectorIndexConfig, keys: &[u32]) {
        let index = Index::new(&options(config)).unwrap();
        index.reserve(keys.len().max(1)).unwrap();
        for (i, &key) in keys.iter().enumerate() {
            index
                .add(key as u64, &test_vector(config.dim, i + 1))
                .unwrap();
        }
        let path = segments_dir(root).join(format!("segment-{id}.usearch"));
        index.save(path.to_str().unwrap()).unwrap();
        write_keys(&segment_keys_path(root, id), keys).unwrap();
    }

    #[test]
    fn create_then_open_empty_layout() {
        let dir = TempDir::new("create-open");
        let config = test_config();
        let engine = UsearchEngine::create(&dir.0, config.clone()).unwrap();
        drop(engine);

        // ADR 0004 §1: the empty layout is durable from create — the RAM
        // snapshot pair (the empty snapshot carries the dimension, task
        // 3.3 revision) and the segment directory.
        assert!(dir.0.join("ram.keys").is_file(), "empty ram.keys manifest");
        assert!(
            dir.0.join("ram.usearch").is_file(),
            "the create-time empty snapshot exists"
        );
        assert!(dir.0.join("segments").is_dir(), "segments directory");
        assert!(
            read_keys(&dir.0.join("ram.keys")).unwrap().is_empty(),
            "the manifest starts empty"
        );

        // A second create on the same layout fails (use open for that).
        assert!(UsearchEngine::create(&dir.0, config.clone()).is_err());

        // open returns an empty engine (count = 0), no error.
        let engine = UsearchEngine::open(&dir.0, config).unwrap();
        assert_eq!(engine.count().unwrap(), 0);
        assert!(engine.chunk_ids().unwrap().is_empty());
    }

    /// Task 3.3 revision (regression guard): the dimension of an EMPTY
    /// created index is durable on disk — reopening with a different
    /// dimension fails with [`VectorsError::DimensionMismatch`]. Before
    /// the fix `create` wrote only the (empty) `ram.keys` manifest; the
    /// `ram.usearch` snapshot appeared at the first save, so an 8-dim and
    /// a 4-dim empty index were byte-identical on disk and `open` with a
    /// different dimension silently succeeded.
    #[test]
    fn empty_created_index_dimension_is_durable() {
        let dir = TempDir::new("empty-dim");
        let config = VectorIndexConfig::new(8, 4, 8, 1, 1, 8).expect("dim-8 config");
        let engine = UsearchEngine::create(&dir.0, config.clone()).unwrap();
        assert_eq!(engine.count().unwrap(), 0, "the created index is empty");
        drop(engine);

        // A dim-4 config against the stored dim-8 layout must fail.
        let other = VectorIndexConfig::new(4, 4, 8, 1, 1, 8).expect("dim-4 config");
        match UsearchEngine::open(&dir.0, other) {
            Err(VectorsError::DimensionMismatch { expected, actual }) => {
                assert_eq!((expected, actual), (4, 8));
            }
            Ok(_) => panic!("expected DimensionMismatch, got an opened engine"),
            Err(other) => panic!("expected DimensionMismatch, got: {other:?}"),
        }
    }

    /// Task 3.3 revision: an empty `rebuild` (0 rows) leaves a
    /// dim-detectable layout — the saved snapshot carries the dimension,
    /// so a later `open` with a different dimension still fails.
    #[test]
    fn empty_rebuild_leaves_dim_detectable_layout() {
        let dir = TempDir::new("empty-rebuild");
        let config = VectorIndexConfig::new(8, 4, 8, 1, 1, 8).expect("dim-8 config");
        let engine = UsearchEngine::create(&dir.0, config.clone()).unwrap();
        engine
            .rebuild(&[(1, test_vector(8, 1)), (2, test_vector(8, 2))])
            .unwrap();
        // The empty rebuild: rows replaced by nothing, state persisted.
        engine.rebuild(&[]).unwrap();
        assert_eq!(engine.count().unwrap(), 0);
        assert!(dir.0.join("ram.usearch").is_file(), "snapshot persisted");
        drop(engine);

        let other = VectorIndexConfig::new(4, 4, 8, 1, 1, 8).expect("dim-4 config");
        assert!(
            matches!(
                UsearchEngine::open(&dir.0, other),
                Err(VectorsError::DimensionMismatch { .. })
            ),
            "an empty rebuild must leave the dimension detectable"
        );
    }

    #[test]
    fn open_missing_path_is_not_found() {
        let dir = TempDir::new("open-missing");
        let config = test_config();

        // A path that does not exist at all.
        match UsearchEngine::open(dir.0.join("nope"), config.clone()) {
            Err(VectorsError::NotFound(_)) => {}
            Ok(_) => panic!("expected NotFound, got an opened engine"),
            Err(other) => panic!("expected NotFound, got: {other:?}"),
        }
        // An existing directory without a layout.
        match UsearchEngine::open(&dir.0, config) {
            Err(VectorsError::NotFound(_)) => {}
            Ok(_) => panic!("expected NotFound, got an opened engine"),
            Err(other) => panic!("expected NotFound, got: {other:?}"),
        }
    }

    #[test]
    fn garbage_cleanup_on_open() {
        let dir = TempDir::new("garbage");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();

        // A valid segment must survive the cleanup.
        write_segment(&dir.0, 2, &config, &[7]);

        // Crash garbage (ADR 0004 §7 crash matrix).
        let tmp_dir = dir.0.join("segments.tmp");
        std::fs::create_dir_all(&tmp_dir).unwrap();
        std::fs::write(tmp_dir.join("segment-9.usearch"), b"garbage").unwrap();
        std::fs::write(dir.0.join("segment-1.tmp"), b"garbage").unwrap();
        std::fs::write(dir.0.join("stale.old"), b"garbage").unwrap();

        let engine = UsearchEngine::open(&dir.0, config.clone()).unwrap();

        assert!(!dir.0.join("segments.tmp").exists(), "segments.tmp removed");
        assert!(!dir.0.join("segment-1.tmp").exists(), "*.tmp removed");
        assert!(!dir.0.join("stale.old").exists(), "*.old removed");
        assert!(
            dir.0.join("segments").join("segment-2.usearch").is_file(),
            "valid segment intact"
        );

        // The restored DISK segment is searchable end-to-end (mmap view):
        // key 7 holds the axis-0 unit vector, the query is axis 1, so the
        // L2sq distance is exactly 2.0.
        let results = engine.search(&test_vector(8, 2), 1).unwrap();
        assert_eq!(results, vec![(7, 2.0)]);
    }

    #[test]
    fn garbage_cleanup_promotes_segments_tmp() {
        let dir = TempDir::new("promote");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();

        // Simulate the crash between the two directory renames (ADR 0004
        // §7): segments/ is gone, the compaction result sits in segments.tmp/.
        std::fs::remove_dir_all(dir.0.join("segments")).unwrap();
        let tmp_dir = dir.0.join("segments.tmp");
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let index = Index::new(&options(&config)).unwrap();
        index.reserve(1).unwrap();
        index.add(3, &test_vector(8, 3)).unwrap();
        index
            .save(tmp_dir.join("segment-1.usearch").to_str().unwrap())
            .unwrap();
        write_keys(&tmp_dir.join("segment-1.keys"), &[3]).unwrap();

        let engine = UsearchEngine::open(&dir.0, config.clone()).unwrap();

        assert!(
            !dir.0.join("segments.tmp").exists(),
            "the scratch directory is promoted, not deleted"
        );
        assert!(
            dir.0.join("segments").join("segment-1.usearch").is_file(),
            "promoted segment intact"
        );
        let results = engine.search(&test_vector(8, 3), 1).unwrap();
        assert_eq!(results.first().map(|(id, _)| *id), Some(3));
    }

    #[test]
    fn open_missing_sidecar_is_distinct_error() {
        let dir = TempDir::new("missing-sidecar");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();

        // A segment index file WITHOUT its sidecar manifest (ADR 0004 §3:
        // both files of a pair must exist).
        let segments = dir.0.join("segments");
        let index = Index::new(&options(&config)).unwrap();
        index.reserve(1).unwrap();
        index.add(1, &test_vector(8, 1)).unwrap();
        index
            .save(segments.join("segment-1.usearch").to_str().unwrap())
            .unwrap();

        match UsearchEngine::open(&dir.0, config) {
            Err(VectorsError::NotFound(path)) => {
                assert!(
                    path.contains("segment-1.keys"),
                    "the error must name the missing manifest: {path}"
                );
            }
            Ok(_) => panic!("expected a distinct error, got an opened engine"),
            Err(other) => panic!("expected a distinct error, got: {other:?}"),
        }
    }

    #[test]
    fn open_corrupt_sidecar_is_distinct_error() {
        let dir = TempDir::new("corrupt-sidecar");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();

        let segments = dir.0.join("segments");
        let index = Index::new(&options(&config)).unwrap();
        index.reserve(1).unwrap();
        index.add(1, &test_vector(8, 1)).unwrap();
        index
            .save(segments.join("segment-1.usearch").to_str().unwrap())
            .unwrap();
        // A manifest with a corrupted magic (8 zero bytes).
        std::fs::write(segments.join("segment-1.keys"), [0u8; 8]).unwrap();

        assert!(
            matches!(
                UsearchEngine::open(&dir.0, config),
                Err(VectorsError::KeysBadMagic)
            ),
            "a corrupted magic must be a distinct KeysBadMagic error, not a panic"
        );
    }

    #[test]
    fn open_dim_mismatch_is_distinct_error() {
        let dir = TempDir::new("dim-mismatch");
        let config = test_config(); // dim 8
        UsearchEngine::create(&dir.0, config.clone()).unwrap();

        // A segment file built with another dimality (16).
        let other = VectorIndexConfig::new(16, 4, 8, 1, 1, 8).expect("other-dim config");
        write_segment(&dir.0, 1, &other, &[1, 2]);

        match UsearchEngine::open(&dir.0, config) {
            Err(VectorsError::DimensionMismatch { expected, actual }) => {
                assert_eq!((expected, actual), (8, 16));
            }
            Ok(_) => panic!("expected DimensionMismatch, got an opened engine"),
            Err(other) => panic!("expected DimensionMismatch, got: {other:?}"),
        }
    }

    #[test]
    fn wal_reconciliation_drops_orphan_rows() {
        let dir = TempDir::new("wal-reconcile");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();

        // Segment 2 exists on disk; segment 5 does not (orphan rows).
        write_segment(&dir.0, 2, &config, &[10, 11]);

        let db_path = dir.0.join("knowledge.db");
        let conn = Connection::open(&db_path).unwrap();
        create_wal_table(&conn);
        insert_wal_row(&conn, 5, 1, 2); // orphan: no segment-5 files
        insert_wal_row(&conn, 2, 2, 2); // live: segment-2 exists
        insert_wal_row(&conn, 2, 3, 2);
        insert_wal_row(&conn, 0, 4, 2); // RAM (segment 0): always kept
        drop(conn);

        let engine =
            UsearchEngine::open_with_wal(&dir.0, config.clone(), Some(db_path.as_path())).unwrap();

        let conn = Connection::open(&db_path).unwrap();
        assert_eq!(
            wal_rows(&conn),
            vec![(0, 4), (2, 2), (2, 3)],
            "orphan rows are removed, existing-segment and RAM rows are kept"
        );
        drop(conn);

        // The raw stale sets are loaded at open (ADR 0004 §8 step 6).
        let mut expected: HashMap<u32, HashSet<u32>> = HashMap::new();
        expected.insert(2, HashSet::from([2, 3]));
        expected.insert(0, HashSet::from([4]));
        assert_eq!(engine.stale_sets(), expected);

        // The WAL write path stays attached after open.
        engine.insert(50, &test_vector(8, 1)).unwrap();
        let conn = Connection::open(&db_path).unwrap();
        assert!(
            wal_rows(&conn).contains(&(0, 50)),
            "the insert must be journaled to the attached WAL"
        );
        drop(conn);
    }

    #[test]
    fn rebuild_clears_disk_and_wal() {
        let dir = TempDir::new("rebuild");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();

        // Three DISK segments + WAL rows for all of them and for RAM.
        write_segment(&dir.0, 1, &config, &[1, 2]);
        write_segment(&dir.0, 2, &config, &[3]);
        write_segment(&dir.0, 3, &config, &[4, 5]);

        let db_path = dir.0.join("knowledge.db");
        let conn = Connection::open(&db_path).unwrap();
        create_wal_table(&conn);
        for &(segment_id, chunk_id) in &[(1u32, 1u32), (1, 2), (2, 3), (3, 4), (3, 5), (0, 9)] {
            insert_wal_row(&conn, segment_id, chunk_id, 2);
        }
        drop(conn);

        let engine =
            UsearchEngine::open_with_wal(&dir.0, config.clone(), Some(db_path.as_path())).unwrap();

        let rows: Vec<(u32, Vec<f32>)> = (100..103)
            .map(|id| (id, test_vector(8, id as usize)))
            .collect();
        engine.rebuild(&rows).unwrap();

        // 0 DISK segments on disk: the old layers must not survive (ADR
        // 0004 §4, defect #8 closed).
        let remaining: Vec<String> = std::fs::read_dir(dir.0.join("segments"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            remaining.is_empty(),
            "old DISK layers must not survive the rebuild: {remaining:?}"
        );

        // 0 WAL rows.
        let conn = Connection::open(&db_path).unwrap();
        assert!(wal_rows(&conn).is_empty(), "the WAL must be cleared");
        drop(conn);

        // RAM = rows only; the stale cache is reset with the WAL.
        assert_eq!(engine.count().unwrap(), rows.len() as u64);
        assert!(
            engine.stale_sets().is_empty(),
            "the stale sets must be reset with the WAL"
        );
    }

    #[test]
    fn ram_restore_round_trip() {
        let dir = TempDir::new("ram-restore");
        let config = test_config();
        let engine = UsearchEngine::create(&dir.0, config.clone()).unwrap();
        engine
            .insert_batch(&[(1, &test_vector(8, 1)), (2, &test_vector(8, 2))])
            .unwrap();
        engine.save().unwrap();

        // The snapshot + sidecar are on disk; no atomic-write residue.
        assert!(dir.0.join("ram.usearch").is_file(), "snapshot saved");
        assert_eq!(
            read_keys(&dir.0.join("ram.keys")).unwrap(),
            vec![1, 2],
            "the sidecar tracks the RAM keys"
        );
        let residue: Vec<String> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp") || name.ends_with(".old"))
            .collect();
        assert!(residue.is_empty(), "no atomic-write residue: {residue:?}");

        // The reopen restores the RAM layer into an in-memory copy.
        let reopened = UsearchEngine::open(&dir.0, config).unwrap();
        assert_eq!(reopened.count().unwrap(), 2);
        let results = reopened.search(&test_vector(8, 2), 1).unwrap();
        assert_eq!(results.first().map(|(id, _)| *id), Some(2));
    }
}
