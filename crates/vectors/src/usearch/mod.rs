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
//! # WAL journal (ADR 0004 §3/§4)
//!
//! With a SQLite database attached (a dedicated long-lived connection
//! opened by [`UsearchEngine::open_with_wal`] from the knowledge.db path,
//! or an injected one via [`UsearchEngine::with_wal_db`]), mutations are
//! journaled to the `usearch_vectors_log` table (migrations 3+4, db crate)
//! BEFORE the RAM index is mutated (WAL-first): `insert`/`insert_batch`
//! upsert one `(s, K, DEL)` supersession row per DISK segment `s` holding
//! `K`, `delete_by_chunk_ids` upserts `(0, K, DEL)` for keys in RAM plus
//! `(s, K, DEL)` for each DISK segment holding `K`, and `rebuild` clears
//! the table (its result is fully persisted to the layout). Only the DEL
//! flag is ever written — a row means "the key is invalid in that segment"
//! (deleted or superseded by a fresher layer); ADD/UPD are reserved, never
//! written, because the key record lives in the sidecar manifests. Each
//! operation's rows commit in ONE SQLite transaction (atomic supersession,
//! ADR 0004 §3 invariant 3). The table is `PK (segment_id, chunk_id)`;
//! `segment_id` 0 is the RAM layer, `n` a DISK layer. The WAL stores only
//! `chunk_id` + flags — never the vector payload (design: "WAL without
//! vectors"; the payload comes from the chunks table). Without an attached
//! database (the default), every WAL call is a no-op and the engine
//! behaves RAM-only.
//!
//! The durable WAL is mirrored in a versioned in-memory stale-set cache
//! (ADR 0004 §3): loaded with one SQL select on open and bumped by every
//! write transaction, so steady-state `search`/`count`/`chunk_ids` read the
//! cache with zero SQL.
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
//! The usearch Rust bindings expose no "list all keys" API, so every
//! layer's keys are recorded in the sidecar key manifests (the
//! [`keys_manifest`] codec): the RAM manifest is the in-memory `ram_keys`
//! set, each DISK manifest is loaded with its segment at open.
//! [`UsearchEngine::chunk_ids`] and [`UsearchEngine::count`] enumerate the
//! live keys as the union of the manifests minus the per-segment stale sets
//! (ADR 0004 §6) — no index scans and no SQL on the steady-state path.
//! This is the reconciliation primitive of the cascade protocol (design
//! D3), called rarely by the GC, never on the query path.
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

mod compaction;
mod keys_manifest;
mod layout;
mod options;
mod search;
mod wal;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use rusqlite::Connection;
use usearch::Index;

use layout::{
    DiskSegment, RAM_INDEX_FILE, cleanup_garbage, layout_exists, load_disk_segments,
    load_ram_layer, ram_index_path, ram_keys_path, segment_keys_path, segments_dir, to_str,
};
use options::{map_sqlite, map_usearch, options};
use search::{add_rows, build_search_pool};
use wal::{StaleCache, load_stale_sets, reconcile_wal};

use crate::{VectorIndex, VectorIndexConfig, VectorsError};

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
    /// searches clone the current `Arc`s (ADR 0004 §6); `Arc` because the
    /// background compaction thread (task 3.8) swaps the list after
    /// cloning its state.
    disk_segments: Arc<RwLock<Vec<DiskSegment>>>,
    config: VectorIndexConfig,
    /// The engine root directory (ADR 0004 §1 layout).
    root: PathBuf,
    /// Usearch-specific config (max_segment_vectors, compaction threshold, etc.)
    usearch_config: crate::UsearchConfig,
    /// Versioned in-memory stale-set cache (ADR 0004 §3): the in-memory
    /// mirror of the durable WAL — `segment_id → deleted/superseded chunk
    /// ids`, loaded with one SQL select on open and bumped by every write
    /// transaction. Steady-state reads (`search`/`count`/`chunk_ids`) take
    /// a snapshot of the sets: zero SQL on the query path. `Arc` because
    /// the background compaction thread (task 3.8) bumps it after the
    /// post-swap WAL cleanup.
    stale: Arc<StaleCache>,
    /// The SQLite connection holding the `usearch_vectors_log` WAL table
    /// (ADR 0004 §3); `None` (the default) disables the WAL — every WAL
    /// call is a no-op. `Mutex` because `rusqlite::Connection` is `Send`
    /// but not `Sync`, while the engine is shared across threads
    /// (`VectorIndex: Send + Sync`). The `Connection` is heap-allocated
    /// (`Box`) so the optional WAL stays out of the inline struct size —
    /// without it the `VectorEngine` enum (Lance vs Usearch variants) would
    /// trip `clippy::large_enum_variant`. `Arc<Mutex<Option<..>>>` because
    /// the background compaction thread (task 3.8) cleans the DISK rows
    /// after the directory swap.
    wal: Arc<Mutex<Option<Box<Connection>>>>,
    /// The dedicated rayon search pool sized by
    /// `UsearchConfig::search_threads` (ADR 0004 §6/§9: `search` runs its
    /// per-layer HNSW queries here, off the global pool — the superseded
    /// implementation ignored the config, defect #9). Built in
    /// `create_with_config`/`open_with_wal` from the validated thread
    /// count.
    search_pool: rayon::ThreadPool,
    /// The layout lock (ADR 0004 §5/§7): serializes the structural layout
    /// operations — the flush (task 3.7) and the background compaction
    /// (task 3.8) — so two of them never compute the same next segment id
    /// or replace the segment list at once. A payload-less `Mutex<()>`
    /// held for the duration of one procedure. `Arc` because the
    /// compaction thread takes it.
    layout_lock: Arc<Mutex<()>>,
    /// Single-flight flag for the background compaction (task 3.8, ADR
    /// 0004 §7): `true` while the repack thread is running; a concurrent
    /// `maybe_compact` is a no-op. The thread clears it on completion
    /// (and on panic, so a failed repack never wedges the trigger).
    compacting: Arc<AtomicBool>,
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
        // ADR 0004 §6/§9: the dedicated search pool, sized by the
        // validated `search_threads` (defect #9 closed).
        let search_pool = build_search_pool(usearch_config.search_threads)?;
        let engine = Self {
            index,
            ram_keys: Mutex::new(HashSet::new()),
            disk_segments: Arc::new(RwLock::new(Vec::new())),
            config,
            root,
            usearch_config,
            stale: Arc::new(StaleCache::empty()),
            wal: Arc::new(Mutex::new(None)),
            search_pool,
            layout_lock: Arc::new(Mutex::new(())),
            compacting: Arc::new(AtomicBool::new(false)),
        };
        // The create-time snapshot: an empty `ram.usearch` (carrying the
        // dimension) + the empty `ram.keys` sidecar. `open`'s dim check
        // then always has a file to read.
        engine.save()?;
        Ok(engine)
    }

    /// Creates a new engine with UsearchConfig and attaches the WAL
    /// database (the create-side counterpart of [`Self::open_with_wal`]):
    /// the empty layout is created and a dedicated long-lived
    /// `rusqlite::Connection` to `wal_db` (the knowledge.db path) is
    /// attached, so the engine journals its mutations to the
    /// `usearch_vectors_log` table from the first insert (module docs:
    /// WAL journal). `None` (the default) disables the WAL.
    ///
    /// The table is created by the knowledge database's migration
    /// (db crate) — this method opens a connection but does not migrate.
    ///
    /// Fails if the layout already exists — use
    /// [`Self::open_with_wal`] for that.
    pub fn create_with_wal(
        path: impl Into<PathBuf>,
        config: VectorIndexConfig,
        usearch_config: crate::UsearchConfig,
        wal_db: Option<&Path>,
    ) -> Result<Self, VectorsError> {
        let engine = Self::create_with_config(path, config, usearch_config)?;
        match wal_db {
            Some(db_path) => {
                let conn = Connection::open(db_path).map_err(map_sqlite)?;
                engine.with_wal_db(conn)
            }
            None => Ok(engine),
        }
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
        //    the versioned stale-set cache (ADR 0004 §8 steps 4/6).
        let (wal, stale) = match wal_db {
            Some(db_path) => {
                let conn = Connection::open(db_path).map_err(map_sqlite)?;
                let disk_ids: Vec<u32> = disk_segments.iter().map(|segment| segment.id).collect();
                reconcile_wal(&conn, &disk_ids)?;
                let stale = load_stale_sets(&conn)?;
                (Some(Box::new(conn)), StaleCache::loaded(stale))
            }
            None => (None, StaleCache::empty()),
        };
        // ADR 0004 §6/§9: the dedicated search pool, sized by the
        // validated `search_threads` (defect #9 closed).
        let search_pool = build_search_pool(usearch_config.search_threads)?;
        Ok(Self {
            index,
            ram_keys: Mutex::new(ram_keys),
            disk_segments: Arc::new(RwLock::new(disk_segments)),
            config,
            root,
            usearch_config,
            stale: Arc::new(stale),
            wal: Arc::new(Mutex::new(wal)),
            search_pool,
            layout_lock: Arc::new(Mutex::new(())),
            compacting: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Attaches the SQLite connection that holds the `usearch_vectors_log`
    /// WAL table and takes ownership of it.
    ///
    /// The table is created by the knowledge database's migration
    /// `3-usearch-vectors-log` (db crate); this method does not open or
    /// migrate anything — the consumer (which owns the database) opens the
    /// connection. On attach it loads the per-segment stale sets from the
    /// WAL into the versioned cache (ADR 0004 §8 step 6), so the steady-state
    /// reads start consistent with the durable log. With the WAL attached,
    /// `insert`/`insert_batch` journal supersession DEL rows,
    /// `delete_by_chunk_ids` journals invalidation DEL rows, and `rebuild`
    /// clears the table — each before the RAM index is mutated (WAL-first,
    /// module docs). Without it (the default from
    /// [`Self::create`]/[`Self::open`]), all WAL calls are no-ops.
    pub fn with_wal_db(self, conn: Connection) -> Result<Self, VectorsError> {
        let stale = load_stale_sets(&conn)?;
        self.stale.replace(stale);
        *mutex_guard(&self.wal) = Some(Box::new(conn));
        Ok(self)
    }

    /// Stores `vector` under `chunk_id` (single-row convenience over
    /// [`Self::insert_batch`]). The vector length must equal `config.dim`.
    pub fn insert(&self, chunk_id: u32, vector: &[f32]) -> Result<(), VectorsError> {
        self.insert_batch(&[(chunk_id, vector)])
    }

    /// Stores many vectors; every row is a single concurrent `add`, chunked
    /// into rayon tasks of `ADD_CHUNK` (the C++ index is concurrent). The
    /// total row count is reserved up front (the 2.26 core rejects
    /// insertions without reserved capacity; reserve is monotonic, so
    /// incremental ingestion never shrinks capacity).
    ///
    /// Every vector must have length `config.dim` (otherwise
    /// [`VectorsError::DimensionMismatch`], and the whole call is rejected
    /// before anything is stored). Uniqueness of `chunk_id` is the caller's
    /// responsibility.
    ///
    /// With a WAL attached ([`Self::with_wal_db`]), the supersession rows
    /// (one `(s, K, DEL)` per DISK segment holding `K`) are journaled in one
    /// transaction BEFORE the index is mutated (module docs: WAL journal);
    /// a transaction failure stores nothing in RAM (WAL-first).
    ///
    /// Re-inserting a key already present in RAM is a supersession: the old
    /// copy is removed and the new one stored (the non-multi index rejects a
    /// duplicate `add`).
    ///
    /// The mutation lands on the RAM layer only; persist it with
    /// [`Self::build_index`] or [`Self::rebuild`] (module docs: on-disk
    /// layout). When the batch pushes the RAM layer to
    /// `UsearchConfig::max_segment_vectors`, it is flushed to a new DISK
    /// segment automatically (ADR 0004 §5, the `flush_ram` procedure).
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
        // WAL-first: the supersession rows commit before the RAM index is
        // mutated, so a crash mid-batch leaves a self-healing log (the
        // payload comes from the chunks table on replay) and a failed
        // transaction stores nothing (ADR 0004 §3 invariant 3).
        let keys: Vec<u32> = rows.iter().map(|(chunk_id, _)| *chunk_id).collect();
        self.insert_supersession(&keys)?;
        // The non-multi RAM index rejects a duplicate `add`, so a
        // supersession removes the old copy first. `remove` on an absent key
        // is a no-op (0 removed), so fresh keys are unaffected.
        for &key in &keys {
            self.index.remove(key as u64).map_err(map_usearch)?;
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
        drop(ram_keys);
        // ADR 0004 §5: overflow — the RAM layer reached the configured
        // segment size; flush it to a new DISK segment (monotonic id,
        // crash-safe ordering in `flush_ram`).
        if self.index.size() >= self.usearch_config.max_segment_vectors {
            self.flush_ram()?;
        }
        Ok(())
    }

    /// Removes the vectors whose chunk id is in `chunk_ids`.
    ///
    /// Idempotent: ids that are not present are silently ignored (the core
    /// reports 0 removed), so calling it again with the same ids — or after
    /// a crash between the vector delete and the SQLite chunk delete
    /// (design D3) — is always safe.
    ///
    /// With a WAL attached ([`Self::with_wal_db`]), the invalidation rows
    /// (`(0, K, DEL)` for keys in RAM plus `(s, K, DEL)` per DISK segment
    /// holding `K`) are journaled in one transaction BEFORE the index is
    /// mutated (module docs: WAL journal); a transaction failure stores
    /// nothing (WAL-first).
    ///
    /// The mutation lands on the RAM layer only (module docs: on-disk
    /// layout): an unsaved delete leaves orphaned vectors after a restart,
    /// which the cascade protocol tolerates and the GC re-detects via
    /// `chunk_ids`/`count`.
    pub fn delete_by_chunk_ids(&self, chunk_ids: &[u32]) -> Result<(), VectorsError> {
        if chunk_ids.is_empty() {
            return Ok(());
        }
        // WAL-first: the invalidation rows commit before the RAM index is
        // mutated (crash-safety as in `insert_batch`, ADR 0004 §3/§4).
        self.delete_invalidations(chunk_ids)?;
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

    /// All live chunk ids (manifests minus the per-segment stale sets, ADR
    /// 0004 §6), in no particular order.
    ///
    /// Reconciliation primitive (design D3): the GC job computes
    /// "index − SQLite → delete" from this listing. The usearch bindings
    /// expose no key-enumeration API, so the keys come from the sidecar
    /// manifests (module docs: key enumeration): the RAM manifest minus
    /// `stale[0]`, unioned with each DISK manifest minus `stale[s]`. No
    /// index scans and no SQL — called rarely by the GC, never on the query
    /// path.
    pub fn chunk_ids(&self) -> Result<Vec<u32>, VectorsError> {
        let stale = self.stale.snapshot();
        let mut ids: HashSet<u32> = HashSet::new();
        // RAM layer (segment 0): live = ram_keys − stale[0].
        let ram_keys = mutex_guard(&self.ram_keys);
        if let Some(ram_stale) = stale.get(&0) {
            for key in ram_keys.iter() {
                if !ram_stale.contains(key) {
                    ids.insert(*key);
                }
            }
        } else {
            ids.extend(ram_keys.iter().copied());
        }
        drop(ram_keys);
        // DISK layers: live = keys(s) − stale[s].
        let segments = disk_segments_read_guard(&self.disk_segments);
        for segment in segments.iter() {
            if let Some(seg_stale) = stale.get(&segment.id) {
                for key in segment.keys.iter() {
                    if !seg_stale.contains(key) {
                        ids.insert(*key);
                    }
                }
            } else {
                ids.extend(segment.keys.iter().copied());
            }
        }
        Ok(ids.into_iter().collect())
    }

    /// Number of live vectors currently stored across the RAM and DISK
    /// layers (ADR 0004 §6): the sidecar manifests minus the per-segment
    /// stale sets. Stale (deleted or superseded) rows are not counted.
    pub fn count(&self) -> Result<u64, VectorsError> {
        Ok(self.chunk_ids()?.len() as u64)
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
    ///
    /// Idempotent: a repeated save rewrites the same snapshot pair, and an
    /// empty RAM layer is a no-op (the create-time or the last flush
    /// already persisted the empty snapshot — ADR 0004 §1).
    pub fn build_index(&self) -> Result<(), VectorsError> {
        if self.index.size() == 0 {
            return Ok(());
        }
        self.save()
    }

    /// ADR 0004 §5: flush the RAM layer into a new DISK segment — the
    /// overflow procedure (trigger: `RAM.size() ≥ max_segment_vectors`
    /// inside `insert_batch`).
    ///
    /// Ordering (ADR §5, verified against the §7/§8 crash matrix): segment
    /// file → sidecar → WAL delete → RAM reset, under the layout lock
    /// (serializes with the compaction of task 3.8 and with a concurrent
    /// flush — two flushes would otherwise compute the same next id). A
    /// crash between any two steps is recoverable by
    /// [`Self::open_with_wal`]:
    ///
    /// - file without sidecar → the distinct sidecar error (ADR §3: both
    ///   files of a pair must exist);
    /// - sidecar without the WAL delete / RAM reset → the old RAM snapshot
    ///   still holds the flushed keys: the freshest-wins merge and the key
    ///   union of `count`/`chunk_ids` keep the state consistent;
    /// - WAL delete without the RAM reset → the same, with the redundant
    ///   segment-0 rows already gone.
    ///
    /// The flushed segment is appended to the DISK segment list (a read-only
    /// mmap view of the just-written file), so it is immediately available
    /// to search.
    fn flush_ram(&self) -> Result<(), VectorsError> {
        // The layout lock (ADR §5): one structural procedure at a time.
        let _layout = mutex_guard(&self.layout_lock);
        // 1. n = max(id) + 1 — monotonic, never renumbered (ADR §2).
        let next_id = {
            let segments = disk_segments_read_guard(&self.disk_segments);
            segments
                .iter()
                .map(|segment| segment.id)
                .max()
                .map(|id| id.saturating_add(1))
                .unwrap_or(1)
        };
        // 2. save() the RAM index → segments/segment-n.usearch (tmp +
        //    rename) + the sidecar manifest from ram_keys (ADR §5 step 2).
        let dir = segments_dir(&self.root);
        let index_path = dir.join(format!("segment-{next_id}.usearch"));
        let tmp = dir.join(format!("segment-{next_id}.usearch.tmp"));
        self.index.save(&to_str(&tmp)?).map_err(map_usearch)?;
        rename_over(&tmp, &index_path)?;
        let mut keys: Vec<u32> = mutex_guard(&self.ram_keys).iter().copied().collect();
        keys.sort_unstable();
        keys_manifest::write_keys(&segment_keys_path(&self.root, next_id), &keys)?;
        // The flushed segment is immediately available to search: map the
        // just-written file read-only (ADR §1: DISK layers are views) and
        // append it to the list (the write lock excludes a concurrent
        // rebuild/compaction swap).
        {
            let view = Index::restore_view(&to_str(&index_path)?).map_err(map_usearch)?;
            let segment = DiskSegment {
                id: next_id,
                index: Arc::new(view),
                keys: keys.iter().copied().collect(),
            };
            disk_segments_write_guard(&self.disk_segments).push(segment);
        }
        // 3. WAL: the segment-0 rows are redundant now — their keys are
        //    physically absent from the flushed file, and the older-segment
        //    supersessions live in their own rows (ADR §5 step 3, §3).
        self.flush_wal_ram_rows()?;
        // 4. RAM: reset, clear the manifest, persist the empty snapshot
        //    pair (ADR §5 step 4: the empty snapshot carries the
        //    dimension).
        self.index.reset().map_err(map_usearch)?;
        self.index.reserve(1).map_err(map_usearch)?;
        mutex_guard(&self.ram_keys).clear();
        self.save()?;
        // 5. Bump the stale cache: the segment-0 rows are gone (ADR §5
        //    step 5).
        self.stale.apply(|sets| {
            sets.remove(&0);
        });
        Ok(())
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
        // 4. Persist the new state (ADR 0004 §4: save ram + sidecar). The
        //    stale cache was already reset with the WAL at step 1.
        self.save()?;
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

    /// Snapshot of the versioned per-segment stale-set cache (ADR 0004 §3):
    /// `segment_id → deleted/superseded chunk ids`. Empty when no WAL
    /// database is attached.
    pub fn stale_sets(&self) -> HashMap<u32, HashSet<u32>> {
        self.stale.snapshot()
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

    fn maybe_compact(&self) -> Result<(), VectorsError> {
        UsearchEngine::maybe_compact(self)
    }
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

#[cfg(test)]
mod test_util {
    //! Shared test fixtures: a unique temporary directory that removes
    //! itself (and its contents) when dropped, plus the small config /
    //! vector / WAL-table / DISK-segment helpers shared by the layout and
    //! WAL test modules.

    // Test code: unwrap/expect are intentional (the fixtures are deterministic).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use rusqlite::{Connection, params};
    use usearch::Index;

    use super::keys_manifest::write_keys;
    use super::layout::{segment_keys_path, segments_dir};
    use super::options::options;
    use crate::VectorIndexConfig;

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

    /// A small, fast test config (dim 8, minimal HNSW parameters).
    pub(crate) fn test_config() -> VectorIndexConfig {
        VectorIndexConfig::new(8, 4, 8, 1, 1, 8).expect("valid test config")
    }

    /// A deterministic test vector: unit vector on axis `axis`.
    pub(crate) fn test_vector(dim: usize, axis: usize) -> Vec<f32> {
        let mut vector = vec![0.0f32; dim];
        vector[axis % dim] = 1.0;
        vector
    }

    /// Creates the ADR 0004 WAL table (migrations 3+4 shape) in `conn`.
    pub(crate) fn create_wal_table(conn: &Connection) {
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

    /// Inserts one WAL row (flags: 1 = ADD, 2 = DEL, 4 = UPD).
    pub(crate) fn insert_wal_row(conn: &Connection, segment_id: u32, chunk_id: u32, flags: u8) {
        conn.execute(
            "INSERT OR REPLACE INTO usearch_vectors_log (segment_id, chunk_id, flags, created_at)
             VALUES (?1, ?2, ?3, datetime('now'))",
            params![segment_id as i64, chunk_id as i64, flags as i64],
        )
        .unwrap();
    }

    /// The WAL rows as `(segment_id, chunk_id)` pairs, ordered.
    pub(crate) fn wal_rows(conn: &Connection) -> Vec<(i64, i64)> {
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
    pub(crate) fn write_segment(root: &Path, id: u32, config: &VectorIndexConfig, keys: &[u32]) {
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
}

#[cfg(test)]
mod flush_tests {
    //! Flush-on-overflow and shutdown-save tests (usearch-wal-persistence
    //! task 3.7, ADR 0004 §5/§7/§8): the overflow trigger, the flush
    //! ordering crash matrix, the segment-0 WAL cleanup, and the
    //! idempotent shutdown save point.

    // Test code: unwrap/expect are intentional (the fixtures are deterministic).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashSet;
    use std::path::PathBuf;

    use rusqlite::Connection;

    use super::UsearchEngine;
    use super::keys_manifest::read_keys;
    use super::test_util::{TempDir, create_wal_table, test_config, test_vector, wal_rows};
    use crate::{UsearchConfig, VectorIndexConfig, VectorsError};

    /// A test config (dim 8) with `max_segment_vectors = max` and the
    /// given `ef_search`, as the pair [`UsearchEngine::create_with_config`]
    /// and the `VectorIndexConfig.usearch` section expect. The HNSW graph
    /// quality is raised (m = 16, efConstruction = 100) so the small
    /// fixtures get full recall in the k = N "find everything" searches.
    fn small_config(max: usize, ef_search: usize) -> (VectorIndexConfig, UsearchConfig) {
        let mut config = test_config();
        config.m = 16;
        config.ef_construction = 100;
        config.ef_search = ef_search;
        let usearch = UsearchConfig {
            max_segment_vectors: max,
            ..UsearchConfig::default()
        };
        config.usearch = Some(usearch.clone());
        (config, usearch)
    }

    /// Inserts `ids` as axis unit vectors in one batch.
    fn insert_axis_rows(engine: &UsearchEngine, ids: impl Iterator<Item = u32>) {
        let rows: Vec<(u32, Vec<f32>)> = ids.map(|id| (id, test_vector(8, id as usize))).collect();
        let refs: Vec<(u32, &[f32])> = rows
            .iter()
            .map(|(id, vector)| (*id, vector.as_slice()))
            .collect();
        engine.insert_batch(&refs).unwrap();
    }

    /// A WAL database (table created) at `dir/knowledge.db`.
    fn wal_db(dir: &TempDir) -> PathBuf {
        let path = dir.0.join("knowledge.db");
        let conn = Connection::open(&path).unwrap();
        create_wal_table(&conn);
        drop(conn);
        path
    }

    /// ADR 0004 §5: with `max_segment_vectors = 100`, inserting 150 rows
    /// flushes the first 100 to segment-1 and keeps the last 50 in RAM;
    /// the search spans both layers and finds every row.
    #[test]
    fn overflow_flush_creates_disk_segment_and_search_spans_layers() {
        let dir = TempDir::new("overflow-flush");
        let (config, usearch_config) = small_config(100, 512);
        let engine = UsearchEngine::create_with_config(&dir.0, config, usearch_config).unwrap();

        // Three batches of 50: batch 2 pushes the RAM layer to 100
        // (>= max_segment_vectors) -> flush; batch 3 lands in the fresh
        // RAM layer.
        insert_axis_rows(&engine, 1..=50);
        insert_axis_rows(&engine, 51..=100);
        insert_axis_rows(&engine, 101..=150);

        // One DISK segment (segment-1) with the first 100 keys.
        let segments = dir.0.join("segments");
        assert!(
            segments.join("segment-1.usearch").is_file(),
            "segment-1 index"
        );
        assert_eq!(
            read_keys(&segments.join("segment-1.keys")).unwrap(),
            (1..=100).collect::<Vec<u32>>(),
            "the sidecar holds the flushed keys"
        );
        // RAM holds the remaining 50.
        assert_eq!(engine.index.size(), 50, "RAM size after the flush");
        // count/chunk_ids span RAM + DISK (manifests, no stale rows yet).
        assert_eq!(engine.count().unwrap(), 150);
        let mut ids = engine.chunk_ids().unwrap();
        ids.sort_unstable();
        assert_eq!(ids, (1..=150).collect::<Vec<u32>>());

        // Search spans both layers: every inserted key is found (k = 150;
        // the high ef_search of the test config gives full recall here).
        let results = engine.search(&test_vector(8, 1), 150).unwrap();
        assert_eq!(results.len(), 150, "every inserted key is found");
        let found: HashSet<u32> = results.iter().map(|(id, _)| *id).collect();
        assert_eq!(found, (1..=150).collect::<HashSet<_>>());
    }

    /// ADR 0004 §5 step 3: the flush deletes every segment-0 WAL row —
    /// the RAM-layer invalidations become redundant once the RAM layer is
    /// a DISK segment.
    #[test]
    fn flush_deletes_wal_segment0_rows() {
        let dir = TempDir::new("flush-wal");
        let (config, usearch_config) = small_config(100, 8);
        let db_path = wal_db(&dir);
        UsearchEngine::create_with_config(&dir.0, config.clone(), usearch_config).unwrap();
        let engine = UsearchEngine::open_with_wal(&dir.0, config, Some(db_path.as_path())).unwrap();

        // Batch 1 (1..=100) flushes to segment-1; batch 2 (101..=150) in RAM.
        insert_axis_rows(&engine, 1..=100);
        insert_axis_rows(&engine, 101..=150);
        // Delete 11 RAM keys: the WAL gets (0, k, DEL) rows.
        let deleted: Vec<u32> = (101..=111).collect();
        engine.delete_by_chunk_ids(&deleted).unwrap();
        let conn = Connection::open(&db_path).unwrap();
        assert_eq!(
            wal_rows(&conn),
            (101..=111).map(|key| (0, key as i64)).collect::<Vec<_>>(),
            "the deletes journal segment-0 rows"
        );
        drop(conn);

        // Batch 3 (151..=211, 61 keys) pushes RAM to 100 -> the second
        // flush must delete the segment-0 rows.
        insert_axis_rows(&engine, 151..=211);

        let segments = dir.0.join("segments");
        assert!(
            segments.join("segment-1.usearch").is_file(),
            "segment-1 intact"
        );
        assert!(
            segments.join("segment-2.usearch").is_file(),
            "segment-2 flushed"
        );
        let conn = Connection::open(&db_path).unwrap();
        assert!(
            wal_rows(&conn).is_empty(),
            "the flush must delete the segment-0 rows: {:?}",
            wal_rows(&conn)
        );
        drop(conn);

        // count = segment-1 (100: 1..=100) + segment-2 (100: the 39
        // surviving RAM keys 112..=150 plus the new 151..=211).
        assert_eq!(engine.count().unwrap(), 200);
        let mut ids = engine.chunk_ids().unwrap();
        ids.sort_unstable();
        assert_eq!(ids, (1..=100).chain(112..=211).collect::<Vec<u32>>());
    }

    /// ADR 0004 §5/§7 crash matrix: a crash between the segment file and
    /// the sidecar (or a lost sidecar) leaves an incomplete pair — open
    /// must fail with the distinct sidecar error (ADR §3), never recover
    /// silently.
    #[test]
    fn flush_crash_missing_sidecar_is_distinct_error() {
        // Build the flushed state (segment-1 + 50 RAM keys).
        let dir = TempDir::new("crash-sidecar");
        let (config, usearch_config) = small_config(100, 8);
        let engine =
            UsearchEngine::create_with_config(&dir.0, config.clone(), usearch_config).unwrap();
        insert_axis_rows(&engine, 1..=150);
        drop(engine);

        // The sidecar is gone: the pair is incomplete.
        std::fs::remove_file(dir.0.join("segments").join("segment-1.keys")).unwrap();

        match UsearchEngine::open(&dir.0, config) {
            Err(VectorsError::NotFound(path)) => {
                assert!(
                    path.contains("segment-1.keys"),
                    "the error must name the missing manifest: {path}"
                );
            }
            Ok(_) => panic!("expected a distinct sidecar error, got an opened engine"),
            Err(other) => panic!("expected a distinct sidecar error, got: {other:?}"),
        }
    }

    /// ADR 0004 §5/§7 crash matrix: a lost segment file (the sidecar
    /// survives) is recoverable — open drops the segment with its keys
    /// and the engine stays consistent on the surviving layers.
    #[test]
    fn flush_crash_missing_segment_file_recovers() {
        // With a WAL: the flushed state + a saved RAM snapshot.
        let dir = TempDir::new("crash-segfile");
        let (config, usearch_config) = small_config(100, 64);
        let db_path = wal_db(&dir);
        UsearchEngine::create_with_config(&dir.0, config.clone(), usearch_config).unwrap();
        let engine =
            UsearchEngine::open_with_wal(&dir.0, config.clone(), Some(db_path.as_path())).unwrap();
        // Three batches of 50: batch 2 pushes RAM to 100 -> segment-1;
        // batch 3 leaves the 50 RAM keys to be saved.
        insert_axis_rows(&engine, 1..=50);
        insert_axis_rows(&engine, 51..=100);
        insert_axis_rows(&engine, 101..=150);
        // Persist the RAM snapshot (the shutdown save) so the reopen sees
        // the 50 RAM keys.
        engine.build_index().unwrap();
        drop(engine);

        // The segment file is gone (the sidecar survives).
        std::fs::remove_file(dir.0.join("segments").join("segment-1.usearch")).unwrap();

        let engine = UsearchEngine::open_with_wal(&dir.0, config, Some(db_path.as_path())).unwrap();
        assert_eq!(engine.count().unwrap(), 50, "only the RAM layer survives");
        let mut ids = engine.chunk_ids().unwrap();
        ids.sort_unstable();
        assert_eq!(ids, (101..=150).collect::<Vec<u32>>());
        // The surviving RAM keys are searchable.
        let results = engine.search(&test_vector(8, 101), 50).unwrap();
        let found: HashSet<u32> = results.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            found,
            (101..=150).collect::<HashSet<_>>(),
            "every surviving RAM key is found"
        );
    }

    /// ADR 0004 §4: the shutdown save point (`build_index`) persists the
    /// RAM layer; it is idempotent — a repeated save with no new inserts
    /// is a no-op with no error and no duplicate.
    #[test]
    fn shutdown_save_persists_ram_and_is_idempotent() {
        let dir = TempDir::new("shutdown-save");
        let (config, usearch_config) = small_config(100, 64);
        let engine =
            UsearchEngine::create_with_config(&dir.0, config.clone(), usearch_config).unwrap();
        insert_axis_rows(&engine, 1..=10);
        // The shutdown save point (trait method): persist the RAM layer.
        engine.build_index().unwrap();
        drop(engine);

        // A fresh engine on the same layout sees all 10 rows.
        let reopened = UsearchEngine::open(&dir.0, config.clone()).unwrap();
        assert_eq!(reopened.count().unwrap(), 10);
        let mut ids = reopened.chunk_ids().unwrap();
        ids.sort_unstable();
        assert_eq!(ids, (1..=10).collect::<Vec<u32>>());
        let results = reopened.search(&test_vector(8, 1), 10).unwrap();
        let found: HashSet<u32> = results.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            found,
            (1..=10).collect::<HashSet<_>>(),
            "search finds every saved row"
        );

        // Save again with no new inserts: no error, no duplicate.
        reopened.build_index().unwrap();
        drop(reopened);
        let again = UsearchEngine::open(&dir.0, config).unwrap();
        assert_eq!(
            again.count().unwrap(),
            10,
            "no duplicate after the second save"
        );
        let mut ids = again.chunk_ids().unwrap();
        ids.sort_unstable();
        assert_eq!(ids, (1..=10).collect::<Vec<u32>>());
    }

    /// ADR 0004 §5/§6: `count`/`chunk_ids` across RAM + DISK after a
    /// flush — manifests minus the per-segment stale sets (a superseded
    /// DISK key and a deleted RAM key both drop out exactly once).
    #[test]
    fn count_and_chunk_ids_across_flush() {
        let dir = TempDir::new("count-flush");
        let (config, usearch_config) = small_config(100, 8);
        let db_path = wal_db(&dir);
        UsearchEngine::create_with_config(&dir.0, config.clone(), usearch_config).unwrap();
        let engine = UsearchEngine::open_with_wal(&dir.0, config, Some(db_path.as_path())).unwrap();

        insert_axis_rows(&engine, 1..=100); // flushes to segment-1
        insert_axis_rows(&engine, 101..=150); // RAM
        // Supersede a DISK key (re-insert 50) and delete a RAM key (150).
        engine.insert(50, &test_vector(8, 5)).unwrap();
        engine.delete_by_chunk_ids(&[150]).unwrap();

        let conn = Connection::open(&db_path).unwrap();
        assert_eq!(
            wal_rows(&conn),
            vec![(0, 150), (1, 50)],
            "the supersession + delete rows"
        );
        drop(conn);

        // count = |keys(1) − stale[1]| + |ram − stale[0]|
        //       = (100 − 1) + (51 − 1) = 149.
        assert_eq!(engine.count().unwrap(), 149);
        let mut ids = engine.chunk_ids().unwrap();
        ids.sort_unstable();
        assert_eq!(ids, (1..=149).collect::<Vec<u32>>());
    }
}
