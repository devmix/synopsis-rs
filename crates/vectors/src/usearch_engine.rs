//! USearch-backed ANN engine (add-usearch-ann-engine, task 1.2).
//!
//! [`UsearchEngine`] implements the full [`crate::VectorIndex`] contract on
//! the USearch 2.26 C++11 HNSW core (cxx FFI), mirroring the [`crate::engine::LanceEngine`]
//! semantics:
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
//! - disk-backed serving: [`UsearchEngine::open`] restores the index from
//!   `<dir>/index.usearch` (`Index::restore` loads the file into memory for
//!   read-write) — the opened engine is fully writable;
//! - the HNSW graph is built incrementally during every `add`, so the index
//!   is always search-ready; [`UsearchEngine::build_index`] is the
//!   persistence point (persistence model below).
//!
//! # Persistence model (deliberately differs from `LanceEngine`)
//!
//! A usearch index is one in-memory file, not a versioned dataset:
//! `insert_batch` and `delete_by_chunk_ids` mutate the live index
//! only, and the state is flushed to disk by [`UsearchEngine::build_index`]
//! (plain save) and [`UsearchEngine::rebuild`] (atomic temp-file + rename).
//! This fits the cascade protocol (design D3): an unsaved delete leaves
//! orphaned vectors after a restart, which the consumer filters against
//! SQLite and the GC re-detects via `chunk_ids`/`count`; an unsaved insert
//! is repaired by the full `rebuild` from chunk text (the ultimate repair).
//! A full-file save per ingested document would be O(N²) writes at corpus
//! scale, so persistence is batched at the build/rebuild boundary.
//!
//! # WAL journal (usearch-wal-persistence task 2.2)
//!
//! With a SQLite connection attached via [`UsearchEngine::with_wal_db`],
//! mutations are journaled to the `usearch_vectors_log` table (migration
//! `3-usearch-vectors-log`, db crate) BEFORE the RAM index is mutated
//! (WAL-first): `insert`/`insert_batch` write an ADD record (flag 1),
//! `delete_by_chunk_ids` writes a DEL record (flag 2), and `rebuild`
//! clears the table (its result is fully persisted to the index file).
//! The WAL stores only `chunk_id` + flags — never the vector payload
//! (design: "WAL without vectors"; the payload comes from the chunks
//! table). A crash between the journal write and the RAM mutation leaves
//! a self-healing record. Without an attached connection (the default),
//! every WAL call is a no-op and the engine behaves exactly as before.
//!
//! # Open engines are read-write
//!
//! [`UsearchEngine::open`] loads the index file into memory
//! (`Index::restore`), so an opened engine is fully writable: `insert`,
//! `insert_batch` and `delete_by_chunk_ids` work on it exactly as on a
//! created engine. Mutations still land on the live index only until
//! persisted (persistence model above).
//!
//! # Key enumeration
//!
//! The usearch Rust bindings expose no "list all keys" API, so
//! [`UsearchEngine::chunk_ids`] runs `exact_search` (a guaranteed full
//! brute-force scan) with `k = size()` and a fixed non-zero query: every
//! live vector is a distinct top-k hit (the index is non-multi), so the
//! match keys are exactly the stored chunk ids. Like
//! `LanceEngine::chunk_ids` this is a full scan — the reconciliation
//! primitive of the cascade protocol (design D3), called rarely by the GC,
//! never on the query path.
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
use std::sync::{Mutex, MutexGuard};

use rayon::prelude::*;
use rusqlite::{Connection, params};
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

use crate::{VectorIndex, VectorIndexConfig, VectorsError};

/// Single on-disk index file inside the engine directory (design.md layout:
/// `<vectors_path>/usearch/`).
const INDEX_FILE: &str = "index.usearch";
/// Temp file for the atomic `rebuild` save (same directory as
/// [`INDEX_FILE`] → the final rename is atomic on the same filesystem).
const INDEX_TMP_FILE: &str = "index.usearch.tmp";
/// Rows per rayon task in `insert_batch`/`rebuild` (mirrors the
/// LanceEngine batch size of 1000; each row is a single concurrent `add`).
const ADD_CHUNK: usize = 1000;
/// WAL flag: a vector was added (usearch-wal-persistence design).
const WAL_ADD: u8 = 1;
/// WAL flag: a vector was deleted.
const WAL_DEL: u8 = 2;

/// USearch-backed ANN index (add-usearch-ann-engine task 1.2).
///
/// Owns a usearch `Index` (`Send + Sync`; every mutating method takes
/// `&self` — the C++ index is concurrent by design) and the on-disk file it
/// was created or restored from. All methods are sync; the engine is safe
/// to share across threads and `spawn_blocking` workers.
///
/// Implements [`VectorIndex`] by pure delegation to the inherent methods,
/// so `Arc<dyn VectorIndex>` and `UsearchEngine` are interchangeable.
pub struct UsearchEngine {
    index: Index,
    /// DISK segments (read-only mmap, segment_id = 1..N)
    disk_segments: Vec<Index>,
    config: VectorIndexConfig,
    /// The `<dir>/index.usearch` file backing the index.
    path: PathBuf,
    /// Usearch-specific config (max_segment_vectors, compaction threshold, etc.)
    usearch_config: crate::UsearchConfig,
    /// The SQLite connection holding the `usearch_vectors_log` WAL table
    /// (usearch-wal-persistence task 2.2); `None` (the default) disables
    /// the WAL — every WAL call is a no-op. `Mutex` because
    /// `rusqlite::Connection` is `Send` but not `Sync`, while the engine
    /// is shared across threads (`VectorIndex: Send + Sync`).
    wal: Option<Mutex<Connection>>,
}

impl UsearchEngine {
    /// Creates a new engine with an EMPTY index at `path` (creating the
    /// directory if needed) and persists the empty index to
    /// `<path>/index.usearch`. Fails if that file already exists — use
    /// [`Self::open`] for that.
    pub fn create(
        path: impl Into<PathBuf>,
        config: VectorIndexConfig,
    ) -> Result<Self, VectorsError> {
        let path = path.into();
        config.validate()?;
        let file = path.join(INDEX_FILE);
        if file.exists() {
            return Err(VectorsError::Engine(format!(
                "index already exists at {}",
                file.display()
            )));
        }
        std::fs::create_dir_all(&path)?;
        let index = Index::new(&options(&config)).map_err(map_usearch)?;
        // The 2.26 core rejects a search that finds no reserved worker
        // thread; reserving 1 slot keeps an empty index searchable (and
        // insertable) from the start.
        index.reserve(1).map_err(map_usearch)?;
        let engine = Self {
            index,
            disk_segments: Vec::new(),
            config,
            path: file,
            usearch_config: crate::UsearchConfig::default(),
            wal: None,
        };
        engine.save()?;
        Ok(engine)
    }

    /// Creates a new engine with UsearchConfig.
    pub fn create_with_config(
        path: impl Into<PathBuf>,
        config: VectorIndexConfig,
        usearch_config: crate::UsearchConfig,
    ) -> Result<Self, VectorsError> {
        let path = path.into();
        config.validate()?;
        let file = path.join(INDEX_FILE);
        if file.exists() {
            return Err(VectorsError::Engine(format!(
                "index already exists at {}",
                file.display()
            )));
        }
        std::fs::create_dir_all(&path)?;
        let index = Index::new(&options(&config)).map_err(map_usearch)?;
        index.reserve(1).map_err(map_usearch)?;
        let engine = Self {
            index,
            disk_segments: Vec::new(),
            config,
            path: file,
            usearch_config,
            wal: None,
        };
        engine.save()?;
        Ok(engine)
    }

    /// Opens an engine at `path` created earlier by [`Self::create`]: the
    /// index file is loaded into memory for read-write (`Index::restore`),
    /// so the opened engine is fully writable (module docs: "Open engines
    /// are read-write").
    ///
    /// Returns [`VectorsError::NotFound`] if no index file exists at
    /// `path`, and [`VectorsError::DimensionMismatch`] if the stored
    /// index's vector dimensionality differs from `config.dim`.
    pub fn open(path: impl Into<PathBuf>, config: VectorIndexConfig) -> Result<Self, VectorsError> {
        let path = path.into();
        config.validate()?;
        let file = path.join(INDEX_FILE);
        if !file.exists() {
            return Err(VectorsError::NotFound(path.display().to_string()));
        }
        let index = Index::restore(&to_str(&file)?).map_err(map_usearch)?;
        if index.dimensions() != config.dim {
            return Err(VectorsError::DimensionMismatch {
                expected: config.dim,
                actual: index.dimensions(),
            });
        }
        Ok(Self {
            index,
            disk_segments: Vec::new(),
            config,
            path: file,
            usearch_config: crate::UsearchConfig::default(),
            wal: None,
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
    /// The mutation lands on the live index only; persist it with
    /// [`Self::build_index`] or [`Self::rebuild`] (module docs: persistence
    /// model).
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
        let disk_results: Vec<Vec<(u32, f32)>> = self
            .disk_segments
            .par_iter()
            .map(|seg| {
                let matches = seg
                    .filtered_search(query, k, |key: u64| !global_stale.contains(&(key as u32)))
                    .map_err(map_usearch)?;
                let mut results = Vec::with_capacity(matches.keys.len());
                for j in 0..matches.keys.len() {
                    results.push((key_to_chunk_id(matches.keys[j])?, matches.distances[j]));
                }
                Ok(results)
            })
            .collect::<Result<Vec<_>, VectorsError>>()?;

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
    /// The mutation lands on the live index only (module docs: persistence
    /// model): an unsaved delete leaves orphaned vectors after a restart,
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

    /// Persists the current index state to `<dir>/index.usearch`.
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

    /// Atomically replaces the entire content with `rows`.
    ///
    /// Strategy: `reset()` clears the live index, the rows are added in
    /// one batch, and the result is saved to a temp file that is renamed
    /// over `<dir>/index.usearch` (atomic on the same filesystem, so a
    /// crash mid-rebuild leaves the previous file intact — the operation is
    /// simply re-run). No content from before the call remains (no
    /// accumulation).
    ///
    /// Every vector must have length `config.dim` (the whole call is
    /// rejected otherwise, before anything is touched). An empty `rows`
    /// slice empties the index. This is the ultimate repair of the cascade
    /// protocol (design D3): rebuild from chunk text after the embedding
    /// layer re-encodes the chunks.
    ///
    /// With a WAL attached ([`Self::with_wal_db`]), the WAL table is
    /// cleared before the rebuild: the rebuilt content is fully persisted
    /// to the index file, so every pre-rebuild record is stale (module
    /// docs: WAL journal).
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
        // The rebuild replaces the entire content and persists it to the
        // index file, so the pre-rebuild WAL is stale: clear it before
        // touching the index (module docs: WAL journal).
        self.clear_wal()?;
        // Clear the live index.
        self.index.reset().map_err(map_usearch)?;
        // Reserve the final size up front (1 slot minimum keeps an empty
        // index searchable and insertable).
        self.index.reserve(rows.len().max(1)).map_err(map_usearch)?;
        let refs: Vec<(u32, &[f32])> = rows
            .iter()
            .map(|(id, vector)| (*id, vector.as_slice()))
            .collect();
        add_rows(&self.index, &refs)?;
        // Atomic replace: save to a temp file, then rename over the index
        // file.
        let tmp = self.path.with_file_name(INDEX_TMP_FILE);
        self.index.save(&to_str(&tmp)?).map_err(map_usearch)?;
        if let Err(original) = std::fs::rename(&tmp, &self.path) {
            // `rename` over an existing target is atomic on Unix but fails
            // on Windows; fall back to remove + rename (a tiny non-atomic
            // window, bounded by the rebuild-repair of the cascade
            // protocol).
            let _ = original;
            std::fs::remove_file(&self.path)?;
            std::fs::rename(&tmp, &self.path)?;
        }
        Ok(())
    }

    /// Persists the index to the on-disk file.
    pub fn save(&self) -> Result<(), VectorsError> {
        self.index.save(&to_str(&self.path)?).map_err(map_usearch)
    }

    /// The index/query configuration this engine was created with.
    pub fn config(&self) -> &VectorIndexConfig {
        &self.config
    }

    /// The on-disk index file backing this engine (`<dir>/index.usearch`).
    pub fn path(&self) -> &Path {
        &self.path
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
    fn compact(&mut self) -> Result<(), VectorsError> {
        // No DISK segments to compact
        if self.disk_segments.is_empty() {
            return Ok(());
        }

        // 1. Load live keys per DISK segment via SQL (O(log N) indexed query)
        let live_keys_per_seg = self.load_live_keys_per_segment()?;

        // 2. Collect live vectors from DISK segments using get().
        //    WAL segment_id = seg_idx + 1 (segment_id=0 is RAM).
        //    Deduplicate by key: later segments win (higher segment_id =
        //    more recent snapshot).
        let dim = self.config.dim;
        let mut seen: HashSet<u32> = HashSet::new();
        let mut live_vectors: Vec<(u32, Vec<f32>)> = Vec::new();
        for (seg_idx, seg) in self.disk_segments.iter().enumerate() {
            let wal_segment_id = (seg_idx as u32) + 1;
            let keys = live_keys_per_seg.get(&wal_segment_id);
            let Some(keys) = keys else {
                continue;
            };
            for id in keys {
                // Skip duplicates: later segment version wins
                if !seen.insert(*id) {
                    continue;
                }
                let mut vector = vec![0.0f32; dim];
                seg.get::<f32>(*id as u64, &mut vector)
                    .map_err(map_usearch)?;
                live_vectors.push((*id, vector));
            }
        }

        // 3. Create new segments (sliced by max_segment_vectors)
        let max = self.usearch_config.max_segment_vectors;
        let mut new_segments = Vec::new();
        for chunk in live_vectors.chunks(max) {
            let new_index = Index::new(&options(&self.config)).map_err(map_usearch)?;
            new_index.reserve(chunk.len()).map_err(map_usearch)?;
            for (id, vector) in chunk {
                new_index.add(*id as u64, vector).map_err(map_usearch)?;
            }
            new_segments.push(new_index);
        }

        // 4. Replace old segments
        self.disk_segments = new_segments;

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
        let total = self.disk_segments.iter().map(|s| s.size()).sum::<usize>();
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
        let conn = wal_guard(wal);
        let mut stmt = conn
            .prepare(
                "SELECT segment_id, chunk_id FROM usearch_vectors_log \
                 WHERE flags = ?1",
            )
            .map_err(map_sqlite)?;
        let rows = stmt
            .query_map(params![WAL_DEL as i64], |row| {
                let segment_id = row.get::<_, i64>(0)?;
                let chunk_id = row.get::<_, i64>(1)?;
                Ok((segment_id, chunk_id))
            })
            .map_err(map_sqlite)?;
        let mut grouped: HashMap<u32, HashSet<u32>> = HashMap::new();
        for row in rows {
            let (segment_id, chunk_id) = row.map_err(map_sqlite)?;
            let segment_id = u32::try_from(segment_id).map_err(|_| {
                VectorsError::Engine(format!("WAL segment_id {segment_id} exceeds u32 range"))
            })?;
            let chunk_id = u32::try_from(chunk_id).map_err(|_| {
                VectorsError::Engine(format!("WAL chunk_id {chunk_id} exceeds u32 range"))
            })?;
            grouped.entry(segment_id).or_default().insert(chunk_id);
        }
        Ok(grouped)
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
/// The manifest is the durable record of every key stored in a segment (the
/// usearch core exposes no key-enumeration API, so the manifest is the only
/// key listing). The engine's on-disk layout (task 3.3) is the codec's first
/// production call site; until then only the tests in this file exercise it,
/// hence the module-level `dead_code` allow.
#[allow(dead_code)]
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

#[cfg(test)]
mod keys_tests {
    //! Sidecar key manifest codec tests (usearch-wal-persistence task 3.2).

    // Test code: unwrap/expect are intentional (the fixtures are deterministic).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::atomic::{AtomicU64, Ordering};

    use super::keys_manifest::{read_keys, write_keys};
    use super::*;

    /// A unique temporary directory that removes itself (and its contents)
    /// when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "synopsis-vectors-keys-test-{}-{tag}-{}",
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
