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
/// WAL flag: a vector was updated (reserved for a future update operation;
/// a stored vector with this bit set is stale until compaction).
const WAL_UPD: u8 = 4;

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

        // Load all WAL entries once, group by segment_id
        let wal_by_segment = self.load_wal_grouped()?;

        // Build cumulative stale sets for each segment
        let mut cumulative_stale: Vec<HashSet<u32>> = Vec::new();
        let mut current_stale = HashSet::new();
        for segment_id in 0..=self.disk_segments.len() {
            if let Some(ids) = wal_by_segment.get(&(segment_id as u32)) {
                current_stale.extend(ids);
            }
            cumulative_stale.push(current_stale.clone());
        }

        // Search DISK segments with cumulative WAL filtering
        let disk_results: Vec<Vec<(u32, f32)>> = self
            .disk_segments
            .par_iter()
            .enumerate()
            .map(|(i, seg)| {
                let stale = &cumulative_stale[i];
                let matches = seg
                    .filtered_search(query, k, |key: u64| !stale.contains(&(key as u32)))
                    .map_err(map_usearch)?;
                let mut results = Vec::with_capacity(matches.keys.len());
                for j in 0..matches.keys.len() {
                    results.push((key_to_chunk_id(matches.keys[j])?, matches.distances[j]));
                }
                Ok(results)
            })
            .collect::<Result<Vec<_>, VectorsError>>()?;

        // Search RAM layer with segment_id=0 WAL
        let ram_stale = &cumulative_stale[0];
        let ram_results = self
            .index
            .filtered_search(query, k, |key: u64| !ram_stale.contains(&(key as u32)))
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

    /// The chunk ids whose WAL record marks them stale — `flags` carries
    /// the DEL or UPD bit (a deleted or updated vector is superseded until
    /// compaction rewrites the segments).
    ///
    /// A no-op returning an empty set when no WAL connection is attached.
    #[cfg(test)]
    pub(crate) fn load_stale_ids(&self) -> Result<HashSet<u32>, VectorsError> {
        self.load_stale_ids_for_segment(0)
    }

    /// Load stale IDs for a specific segment (cumulative: includes all
    /// WAL entries from segment 0..segment_id).
    #[cfg(test)]
    fn load_stale_ids_for_segment(&self, segment_id: u32) -> Result<HashSet<u32>, VectorsError> {
        let Some(wal) = &self.wal else {
            return Ok(HashSet::new());
        };
        let conn = wal_guard(wal);
        let mut stmt = conn
            .prepare(
                "SELECT chunk_id FROM usearch_vectors_log \
                 WHERE segment_id <= ?1 AND (flags & ?2) != 0",
            )
            .map_err(map_sqlite)?;
        let rows = stmt
            .query_map(
                params![segment_id as i64, (WAL_DEL | WAL_UPD) as i64],
                |row| row.get::<_, i64>(0),
            )
            .map_err(map_sqlite)?;
        let mut stale = HashSet::new();
        for row in rows {
            let id = row.map_err(map_sqlite)?;
            let id = u32::try_from(id).map_err(|_| {
                VectorsError::Engine(format!("WAL chunk_id {id} exceeds the u32 range"))
            })?;
            stale.insert(id);
        }
        Ok(stale)
    }

    /// Removes every row from `usearch_vectors_log`.
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

    /// Merge all segments, remove stale vectors, create new segments.
    fn compact(&mut self) -> Result<(), VectorsError> {
        // 1. Load cumulative stale IDs across all segments
        let stale = self.load_all_stale_ids()?;

        // 2. Collect live vectors from RAM index (segment_id=0)
        let mut live_vectors: Vec<(u32, Vec<f32>)> = Vec::new();
        let ram_count = self.index.size();
        if ram_count > 0 {
            let query = vec![1.0; self.config.dim];
            let matches = self
                .index
                .exact_search(&query, ram_count)
                .map_err(map_usearch)?;
            for i in 0..matches.keys.len() {
                let id = key_to_chunk_id(matches.keys[i])?;
                if !stale.contains(&id) {
                    let mut vector = Vec::new();
                    self.index
                        .export(id as u64, &mut vector)
                        .map_err(map_usearch)?;
                    live_vectors.push((id, vector));
                }
            }
        }

        // 3. Collect live vectors from all disk segments
        for seg in &self.disk_segments {
            let count = seg.size();
            if count == 0 {
                continue;
            }
            let query = vec![1.0; self.config.dim];
            let matches = seg.exact_search(&query, count).map_err(map_usearch)?;
            for i in 0..matches.keys.len() {
                let id = key_to_chunk_id(matches.keys[i])?;
                if !stale.contains(&id) {
                    let mut vector = Vec::new();
                    seg.export(id as u64, &mut vector).map_err(map_usearch)?;
                    live_vectors.push((id, vector));
                }
            }
        }

        // 4. Create new segments (sliced by max_segment_vectors)
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

        // 5. Replace old segments
        self.disk_segments = new_segments;

        // 6. Clear WAL
        self.clear_wal()?;

        Ok(())
    }

    /// Calculate percentage of stale vectors.
    fn stale_vector_percentage(&self) -> Result<f64, VectorsError> {
        let total = self.total_vector_count();
        if total == 0 {
            return Ok(0.0);
        }
        let stale = self.load_all_stale_ids()?.len();
        Ok(stale as f64 / total as f64 * 100.0)
    }

    /// Count total vectors across RAM + all disk segments.
    fn total_vector_count(&self) -> usize {
        self.index.size() + self.disk_segments.iter().map(|s| s.size()).sum::<usize>()
    }

    /// Load all stale IDs across all segments.
    fn load_all_stale_ids(&self) -> Result<HashSet<u32>, VectorsError> {
        let Some(wal) = &self.wal else {
            return Ok(HashSet::new());
        };
        let conn = wal_guard(wal);
        let mut stmt = conn
            .prepare("SELECT chunk_id FROM usearch_vectors_log WHERE (flags & ?1) != 0")
            .map_err(map_sqlite)?;
        let rows = stmt
            .query_map(params![(WAL_DEL | WAL_UPD) as i64], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(map_sqlite)?;
        let mut stale = HashSet::new();
        for row in rows {
            let id = row.map_err(map_sqlite)?;
            let id = u32::try_from(id).map_err(|_| {
                VectorsError::Engine(format!("WAL chunk_id {id} exceeds the u32 range"))
            })?;
            stale.insert(id);
        }
        Ok(stale)
    }

    /// Load all WAL entries with DEL|UPD flags, grouped by segment_id.
    /// Returns a map: segment_id → HashSet<chunk_id>.
    fn load_wal_grouped(&self) -> Result<HashMap<u32, HashSet<u32>>, VectorsError> {
        let Some(wal) = &self.wal else {
            return Ok(HashMap::new());
        };
        let conn = wal_guard(wal);
        let mut stmt = conn
            .prepare(
                "SELECT segment_id, chunk_id FROM usearch_vectors_log \
                 WHERE (flags & ?1) != 0",
            )
            .map_err(map_sqlite)?;
        let rows = stmt
            .query_map(params![(WAL_DEL | WAL_UPD) as i64], |row| {
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

    /// Number of clusters in the corpus geometry (the same clustered
    /// geometry as the LanceEngine tests: a center is the strict global
    /// minimum of its cluster, so top-1 assertions are structurally
    /// reliable under HNSW approximation).
    const TEST_CLUSTERS: usize = 8;
    /// Per-dimension Gaussian noise sigma around a cluster center (tight
    /// clusters: intra-cluster L2 ~ 0.23 vs inter-cluster L2 ~ 1.43).
    const TEST_NOISE_SIGMA: f64 = 0.005;
    /// Test vector dimensionality (bge-m3, per design.md).
    const DIM: usize = 1024;

    /// A deterministic Gaussian sampler: SplitMix64 + Box-Muller (no RNG
    /// crate in the frozen palette).
    struct Gauss {
        state: u64,
        spare: Option<f64>,
    }

    impl Gauss {
        fn new(seed: u64) -> Self {
            Self {
                state: seed,
                spare: None,
            }
        }

        fn next_u64(&mut self) -> u64 {
            self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        /// Standard normal sample (Box-Muller), deterministic.
        fn sample(&mut self) -> f64 {
            if let Some(spare) = self.spare.take() {
                return spare;
            }
            const INV_2_POW_53: f64 = 1.1102230246251565e-16;
            let u1 = 1.0 - ((self.next_u64() >> 11) as f64 * INV_2_POW_53);
            let u2 = (self.next_u64() >> 11) as f64 * INV_2_POW_53;
            let r = (-2.0 * u1.ln()).sqrt();
            let theta = std::f64::consts::TAU * u2;
            self.spare = Some(r * theta.sin());
            r * theta.cos()
        }
    }

    /// ADR 0003 dim with small-corpus index parameters. For the usearch
    /// engine the IVF fields (`num_partitions`/`nprobes`) are ignored;
    /// `m`/`ef_construction`/`ef_search` map to the HNSW parameters.
    fn test_config() -> VectorIndexConfig {
        VectorIndexConfig::new(1024, 16, 100, 8, 8, 100).expect("test config is valid")
    }

    /// `n` clustered rows with ids `offset..offset+n` at dim 1024 (the same
    /// geometry as the LanceEngine tests: the first [`TEST_CLUSTERS`] rows
    /// are exact cluster centers — unit basis vectors `e_(2·i)` — every
    /// other row is a noisy member of cluster
    /// `(i - TEST_CLUSTERS) % TEST_CLUSTERS`).
    fn rows(offset: u32, n: usize) -> Vec<(u32, Vec<f32>)> {
        (0..n)
            .map(|i| {
                let id = offset + i as u32;
                let center = if i < TEST_CLUSTERS {
                    i
                } else {
                    (i - TEST_CLUSTERS) % TEST_CLUSTERS
                };
                let mut row = vec![0.0f32; DIM];
                row[center * 2] = 1.0;
                if i >= TEST_CLUSTERS {
                    let mut gauss = Gauss::new(0xA11CE + id as u64);
                    for value in row.iter_mut() {
                        *value += (TEST_NOISE_SIGMA * gauss.sample()) as f32;
                    }
                }
                (id, row)
            })
            .collect()
    }

    fn batch_refs(data: &[(u32, Vec<f32>)]) -> Vec<(u32, &[f32])> {
        data.iter().map(|(id, vec)| (*id, vec.as_slice())).collect()
    }

    /// `UsearchEngine` is not `Debug` (it owns a usearch `Index`), so a
    /// failing construction is unwrapped by hand instead of `expect_err`.
    fn err_of(result: Result<UsearchEngine, VectorsError>) -> VectorsError {
        match result {
            Err(err) => err,
            Ok(_) => panic!("engine construction must fail"),
        }
    }

    /// Acceptance criterion (task 1.1, kept): create -> add 100 vectors
    /// dim=1024 -> search top-10 -> save -> restore -> search gives the
    /// same keys.
    #[test]
    fn create_add_search_save_restore_roundtrip() {
        let dir = TempDir::new();
        let engine = UsearchEngine::create(&dir.0, test_config()).expect("create engine");
        assert_eq!(engine.count().expect("count"), 0, "fresh index is empty");
        assert_eq!(engine.index.dimensions(), DIM);
        assert_eq!(engine.index.metric_kind(), MetricKind::L2sq);
        // The default (absent) quantization resolves to the engine default.
        assert_eq!(engine.index.scalar_kind(), ScalarKind::BF16);

        // 100 deterministic 1024-dim vectors, one per key.
        let vectors: Vec<(u64, Vec<f32>)> =
            (0..100).map(|key| (key, seeded_vector(key, DIM))).collect();
        let refs: Vec<(u32, &[f32])> = vectors
            .iter()
            .map(|(key, vec)| (*key as u32, vec.as_slice()))
            .collect();
        engine.insert_batch(&refs).expect("insert 100 vectors");
        assert_eq!(engine.count().expect("count"), 100);

        // Top-10 for a stored vector: that vector is top-1 at ~0 distance
        // (the query is down-cast to the configured kind exactly like its
        // stored copy) and the results are distance-ascending.
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

        // Restore: the same top-10 keys come back.
        let engine = UsearchEngine::open(&dir.0, test_config()).expect("open");
        assert_eq!(
            engine.count().expect("count"),
            100,
            "the restored engine sees the saved vectors"
        );
        let restored = engine
            .search(&vectors[42].1, 10)
            .expect("search after restore");
        let keys = |results: &[(u32, f32)]| results.iter().map(|(key, _)| *key).collect::<Vec<_>>();
        assert_eq!(
            keys(&results),
            keys(&restored),
            "restore must return the same keys"
        );
    }

    /// The quantization string maps to the expected usearch [`ScalarKind`];
    /// `None` (absent) and `"bf16"` resolve to the engine default, and an
    /// unrecognized value falls back to the default (defense-in-depth).
    #[test]
    fn quantization_maps_configured_kinds() {
        assert_eq!(quantization(None), ScalarKind::BF16);
        assert_eq!(quantization(Some("u8")), ScalarKind::U8);
        assert_eq!(quantization(Some("i8")), ScalarKind::I8);
        assert_eq!(quantization(Some("f16")), ScalarKind::F16);
        assert_eq!(quantization(Some("bf16")), ScalarKind::BF16);
        assert_eq!(quantization(Some("f32")), ScalarKind::F32);
        // Case-insensitive.
        assert_eq!(quantization(Some("BF16")), ScalarKind::BF16);
        // Unrecognized → the BF16 default.
        assert_eq!(quantization(Some("fp8")), ScalarKind::BF16);
    }

    /// An explicit quantization on the config is honored by the created
    /// engine (not just the default).
    #[test]
    fn create_with_explicit_quantization() {
        for (kind, scalar) in [
            ("u8", ScalarKind::U8),
            ("i8", ScalarKind::I8),
            ("f16", ScalarKind::F16),
            ("bf16", ScalarKind::BF16),
            ("f32", ScalarKind::F32),
        ] {
            let dir = TempDir::new();
            let config = VectorIndexConfig::new(1024, 16, 100, 8, 8, 100)
                .expect("valid")
                .with_quantization(kind);
            let engine = UsearchEngine::create(&dir.0, config).expect("create");
            assert_eq!(
                engine.index.scalar_kind(),
                scalar,
                "quantization {kind} must map to {scalar:?}"
            );
        }
    }

    #[test]
    fn create_insert_search_roundtrip() {
        let dir = TempDir::new();
        let engine = UsearchEngine::create(&dir.0, test_config()).expect("create engine");

        let data = rows(0, 2500);
        engine
            .insert_batch(&batch_refs(&data))
            .expect("insert 2500 rows");
        engine.build_index().expect("build (persist) index");

        // Query = an exact stored row (a cluster center): top-1 must be
        // that row at ~0 distance, and the results must be sorted by
        // distance ascending.
        let results = engine.search(&data[3].1, 10).expect("search");
        assert_eq!(results.len(), 10, "10 results for 2500 stored rows");
        assert_eq!(results[0].0, 3, "top-1 must be the queried row");
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

        // A single-row insert after the batch lands and is searchable.
        let extra = seeded_vector(0xDEAD_BEEF, DIM);
        engine.insert(9999, &extra).expect("single insert");
        let results = engine.search(&extra, 5).expect("search after insert");
        assert_eq!(results[0].0, 9999, "freshly inserted row must be top-1");
    }

    #[test]
    fn open_missing_index_is_not_found() {
        let dir = TempDir::new();
        let err = err_of(UsearchEngine::open(&dir.0, test_config()));
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
        UsearchEngine::create(&dir.0, test_config()).expect("first create");
        let err = err_of(UsearchEngine::create(&dir.0, test_config()));
        assert!(matches!(err, VectorsError::Engine(_)), "got {err:?}");
    }

    #[test]
    fn dim_mismatch_and_zero_k_are_rejected() {
        let dir = TempDir::new();
        let engine = UsearchEngine::create(&dir.0, test_config()).expect("create");
        let short = vec![0.0f32; 512];
        match engine.insert(1, &short) {
            Err(VectorsError::DimensionMismatch { expected, actual }) => {
                assert_eq!((expected, actual), (1024, 512));
            }
            other => panic!("expected DimensionMismatch, got {other:?}"),
        }
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

        // A batch with one bad row is rejected as a whole.
        let bad = vec![0.0f32; 3];
        let batch = vec![(1u32, good.as_slice()), (2u32, bad.as_slice())];
        assert!(
            matches!(
                engine.insert_batch(&batch),
                Err(VectorsError::DimensionMismatch { .. })
            ),
            "mixed batch must be rejected"
        );
        assert_eq!(engine.count().expect("count"), 0, "nothing was stored");
    }

    #[test]
    fn search_empty_index_returns_empty() {
        let dir = TempDir::new();
        let engine = UsearchEngine::create(&dir.0, test_config()).expect("create");
        let query = seeded_vector(42, DIM);
        let results = engine.search(&query, 10).expect("search on empty index");
        assert!(
            results.is_empty(),
            "empty index must yield no results, got {results:?}"
        );

        // An empty index persists and reopens as an empty index.
        engine.build_index().expect("persist empty index");
        drop(engine);
        let engine = UsearchEngine::open(&dir.0, test_config()).expect("reopen empty");
        assert_eq!(engine.count().expect("count"), 0);
        assert!(
            engine
                .search(&query, 10)
                .expect("search on reopened empty")
                .is_empty()
        );
    }

    #[test]
    fn open_with_different_dim_is_rejected() {
        let dir = TempDir::new();
        let small = VectorIndexConfig::new(512, 16, 100, 8, 8, 100).expect("valid");
        UsearchEngine::create(&dir.0, small).expect("create with dim 512");

        let err = err_of(UsearchEngine::open(&dir.0, test_config()));
        match err {
            VectorsError::DimensionMismatch { expected, actual } => {
                assert_eq!((expected, actual), (1024, 512));
            }
            other => panic!("expected DimensionMismatch, got {other:?}"),
        }
    }

    #[test]
    fn delete_removes_rows_and_is_idempotent() {
        let dir = TempDir::new();
        let engine = UsearchEngine::create(&dir.0, test_config()).expect("create");
        let data = rows(0, 500);
        engine.insert_batch(&batch_refs(&data)).expect("insert");

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

        // Idempotent: the same ids again, ids that were never stored, and
        // an empty list (a no-op).
        engine
            .delete_by_chunk_ids(&to_delete)
            .expect("repeat delete");
        engine
            .delete_by_chunk_ids(&[9000, 9001])
            .expect("delete of absent ids");
        engine.delete_by_chunk_ids(&[]).expect("empty delete");
        assert_eq!(engine.count().expect("count"), 496);
        let remaining = engine.chunk_ids().expect("chunk_ids");
        assert!(remaining.iter().all(|id| !to_delete.contains(id)));
    }

    #[test]
    fn chunk_ids_and_count_track_insert_and_delete() {
        let dir = TempDir::new();
        let engine = UsearchEngine::create(&dir.0, test_config()).expect("create");
        assert_eq!(engine.count().expect("count"), 0);
        assert!(engine.chunk_ids().expect("chunk_ids").is_empty());

        let data = rows(0, 1200);
        engine.insert_batch(&batch_refs(&data)).expect("insert");
        assert_eq!(engine.count().expect("count"), 1200);
        let mut ids = engine.chunk_ids().expect("chunk_ids");
        ids.sort_unstable();
        let expected: Vec<u32> = (0..1200).collect();
        assert_eq!(ids, expected, "every stored id exactly once");

        engine.delete_by_chunk_ids(&[0, 599, 1199]).expect("delete");
        assert_eq!(engine.count().expect("count"), 1197);
        let ids = engine.chunk_ids().expect("chunk_ids");
        assert_eq!(ids.len(), 1197, "soft-deleted ids must not be enumerated");
        assert!(!ids.contains(&0) && !ids.contains(&599) && !ids.contains(&1199));
    }

    #[test]
    fn rebuild_replaces_content_without_accumulation() {
        let dir = TempDir::new();
        let engine = UsearchEngine::create(&dir.0, test_config()).expect("create");
        let first = rows(0, 300);
        engine
            .insert_batch(&batch_refs(&first))
            .expect("insert first");

        let second = rows(300, 300);
        engine.rebuild(&second).expect("rebuild");

        // No accumulation: exactly the new rows, none of the first set.
        assert_eq!(engine.count().expect("count"), 300);
        let ids = engine.chunk_ids().expect("chunk_ids");
        assert_eq!(ids.len(), 300);
        assert!(ids.iter().all(|id| (300..600).contains(id)));

        // The index is search-ready over the new content: a new center row
        // is top-1 for its own vector, and a query equal to a replaced (old)
        // row must not return the old id.
        let results = engine.search(&second[3].1, 5).expect("search new");
        assert_eq!(results[0].0, 303, "new center row 300+3 must be top-1");
        let stale = engine.search(&first[0].1, 10).expect("search stale query");
        assert!(
            !stale.iter().any(|(id, _)| *id < 300),
            "old ids must be gone: {stale:?}"
        );
        // The same center vector is now stored under the new id 300, so it
        // is the exact top-1 for the stale query.
        assert_eq!(
            stale[0].0, 300,
            "identical vector must be top-1 under new id"
        );

        // Rebuild with the same rows again: no accumulation, same keys.
        engine.rebuild(&second).expect("rebuild again");
        assert_eq!(engine.count().expect("count"), 300);
        let mut again = engine.chunk_ids().expect("chunk_ids");
        again.sort_unstable();
        let mut before = ids;
        before.sort_unstable();
        assert_eq!(
            again, before,
            "rebuild with the same rows yields the same keys"
        );
    }

    /// Empirically pins the zero-row rebuild path (empty rebuild).
    #[test]
    fn rebuild_with_empty_rows_empties_the_index() {
        let dir = TempDir::new();
        let engine = UsearchEngine::create(&dir.0, test_config()).expect("create");
        let data = rows(0, 100);
        engine.insert_batch(&batch_refs(&data)).expect("insert");

        engine.rebuild(&[]).expect("rebuild to empty");
        assert_eq!(engine.count().expect("count"), 0);
        assert!(engine.chunk_ids().expect("chunk_ids").is_empty());
        let query = seeded_vector(1, DIM);
        assert!(engine.search(&query, 5).expect("search").is_empty());

        // The emptied engine stays usable (rebuild reserved 1 slot).
        engine
            .insert(1, &query)
            .expect("insert after empty rebuild");
        assert_eq!(engine.count().expect("count"), 1);
    }

    #[test]
    fn reopen_restores_data() {
        let dir = TempDir::new();
        let data = rows(0, 400);
        let query = data[5].1.clone();

        let engine = UsearchEngine::create(&dir.0, test_config()).expect("create");
        engine.insert_batch(&batch_refs(&data)).expect("insert");
        engine.build_index().expect("persist");
        engine.delete_by_chunk_ids(&[5, 137]).expect("delete");
        engine.build_index().expect("persist after delete");
        let before = engine.search(&query, 100).expect("search");
        drop(engine);

        let engine = UsearchEngine::open(&dir.0, test_config()).expect("reopen");
        assert_eq!(engine.count().expect("count"), 398);
        let ids = engine.chunk_ids().expect("chunk_ids");
        assert!(!ids.contains(&5) && !ids.contains(&137));
        let after = engine.search(&query, 100).expect("search");
        let keys = |results: &[(u32, f32)]| results.iter().map(|(id, _)| *id).collect::<Vec<_>>();
        assert_eq!(
            keys(&before),
            keys(&after),
            "reopen from the saved path must see the same data"
        );
        assert!(!after.iter().any(|(id, _)| *id == 5 || *id == 137));
    }

    /// An engine opened via [`UsearchEngine::open`] is fully read-write:
    /// search, `insert` and `delete_by_chunk_ids` all work on it (the
    /// index is loaded into memory by `Index::restore`), and `rebuild`
    /// still atomically replaces the file.
    #[test]
    fn opened_engine_is_read_write() {
        let dir = TempDir::new();
        let data = rows(0, 100);
        let engine = UsearchEngine::create(&dir.0, test_config()).expect("create");
        engine.insert_batch(&batch_refs(&data)).expect("insert");
        engine.build_index().expect("persist");
        drop(engine);

        let engine = UsearchEngine::open(&dir.0, test_config()).expect("open");
        assert_eq!(engine.count().expect("count"), 100);
        let results = engine.search(&data[1].1, 3).expect("search on opened");
        assert_eq!(results[0].0, 1);

        // The opened engine is writable: insert and delete land on the
        // live index.
        let extra = seeded_vector(7, DIM);
        engine
            .insert(9000, &extra)
            .expect("insert on opened engine");
        assert_eq!(engine.count().expect("count"), 101);
        engine
            .delete_by_chunk_ids(&[1])
            .expect("delete on opened engine");
        assert_eq!(engine.count().expect("count"), 100);
        let results = engine.search(&extra, 3).expect("search for inserted");
        assert_eq!(results[0].0, 9000, "inserted row must be top-1");

        // Rebuild still atomically replaces the file.
        let replacement = rows(0, 50);
        engine
            .rebuild(&replacement)
            .expect("rebuild on opened engine");
        assert_eq!(engine.count().expect("count"), 50);
        engine.insert(9001, &extra).expect("insert after rebuild");
        assert_eq!(engine.count().expect("count"), 51);
        engine.build_index().expect("persist");
        drop(engine);

        let engine = UsearchEngine::open(&dir.0, test_config()).expect("reopen after rebuild");
        assert_eq!(engine.count().expect("count"), 51);
        let ids = engine.chunk_ids().expect("chunk_ids");
        assert!(
            ids.contains(&9001) && ids.iter().all(|id| *id < 50 || *id == 9001),
            "old ids must be gone after the rebuild: {ids:?}"
        );
    }

    /// The full contract exercised through `&dyn VectorIndex` — proves the
    /// delegation impl and the object safety of the trait.
    #[test]
    fn trait_object_delegates_to_engine() {
        let dir = TempDir::new();
        let engine = UsearchEngine::create(&dir.0, test_config()).expect("create");
        let data = rows(0, 200);
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
        let replacement = rows(0, 10);
        index.rebuild(&replacement).expect("rebuild via trait");
        assert_eq!(index.count().expect("count"), 10);
    }

    // --- WAL write path (usearch-wal-persistence task 2.2) -----------------

    /// The `usearch_vectors_log` schema (mirrors migration
    /// `3-usearch-vectors-log`, db crate): inlined here because this crate
    /// is tier 0 and cannot depend on `db`.
    const WAL_SCHEMA: &str = "
        CREATE TABLE usearch_vectors_log (
            segment_id  INTEGER NOT NULL,
            chunk_id    INTEGER NOT NULL,
            flags       INTEGER NOT NULL,
            created_at  TEXT NOT NULL,
            PRIMARY KEY (segment_id, chunk_id)
        );
        CREATE INDEX idx_usearch_vectors_log_flags ON usearch_vectors_log(flags);
        CREATE INDEX idx_usearch_vectors_log_segment ON usearch_vectors_log(segment_id);
    ";

    /// A temp-file WAL database: the engine takes ownership of one
    /// connection, and a second connection is kept for the assertions (the
    /// engine does not expose its connection). Removes the file and its
    /// sidecars on drop.
    struct WalDb {
        path: PathBuf,
        check: Connection,
    }

    impl WalDb {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "synopsis-vectors-usearch-wal-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let conn = Connection::open(&path).expect("open wal db");
            conn.execute_batch(WAL_SCHEMA).expect("create wal schema");
            drop(conn);
            let check = Connection::open(&path).expect("open check connection");
            Self { path, check }
        }

        /// A fresh connection for the engine to own (the schema already
        /// exists in the file).
        fn engine_conn(&self) -> Connection {
            Connection::open(&self.path).expect("open engine wal connection")
        }

        /// All WAL rows as `(chunk_id, flags)`, ordered by chunk_id.
        fn rows(&self) -> Vec<(i64, i64)> {
            let mut stmt = self
                .check
                .prepare("SELECT chunk_id, flags FROM usearch_vectors_log ORDER BY chunk_id")
                .expect("prepare wal read");
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .expect("query wal")
                .map(|row| row.expect("wal row"))
                .collect()
        }
    }

    impl Drop for WalDb {
        fn drop(&mut self) {
            let path = &self.path;
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_file(format!("{}-wal", path.display()));
            let _ = std::fs::remove_file(format!("{}-shm", path.display()));
        }
    }

    /// Acceptance: an insert journals an ADD record per chunk_id into the
    /// WAL table (and updates the RAM index).
    #[test]
    fn test_write_wal_add() {
        let dir = TempDir::new();
        let wal = WalDb::new();
        let engine = UsearchEngine::create(&dir.0, test_config())
            .expect("create")
            .with_wal_db(wal.engine_conn());

        let data = rows(0, 10);
        engine.insert_batch(&batch_refs(&data)).expect("insert");

        let expected: Vec<(i64, i64)> = (0..10).map(|id| (id, WAL_ADD as i64)).collect();
        assert_eq!(
            wal.rows(),
            expected,
            "every inserted chunk gets an ADD record"
        );
        assert_eq!(
            engine.count().expect("count"),
            10,
            "the RAM index is updated too"
        );
    }

    /// Acceptance: a delete journals a DEL record that replaces the
    /// earlier ADD record (INSERT OR REPLACE: one row per chunk_id, the
    /// latest operation wins).
    #[test]
    fn test_write_wal_delete() {
        let dir = TempDir::new();
        let wal = WalDb::new();
        let engine = UsearchEngine::create(&dir.0, test_config())
            .expect("create")
            .with_wal_db(wal.engine_conn());

        let data = rows(0, 10);
        engine.insert_batch(&batch_refs(&data)).expect("insert");
        engine.delete_by_chunk_ids(&[3, 7]).expect("delete");

        let expected: Vec<(i64, i64)> = (0..10)
            .map(|id| {
                (
                    id,
                    if id == 3 || id == 7 { WAL_DEL } else { WAL_ADD } as i64,
                )
            })
            .collect();
        assert_eq!(
            wal.rows(),
            expected,
            "deleted chunks carry the DEL flag, the rest keep ADD"
        );
        assert_eq!(
            engine.count().expect("count"),
            8,
            "the RAM index is updated too"
        );
    }

    /// Acceptance: `clear_wal` empties the table.
    #[test]
    fn test_clear_wal() {
        let dir = TempDir::new();
        let wal = WalDb::new();
        let engine = UsearchEngine::create(&dir.0, test_config())
            .expect("create")
            .with_wal_db(wal.engine_conn());

        let data = rows(0, 10);
        engine.insert_batch(&batch_refs(&data)).expect("insert");
        engine.delete_by_chunk_ids(&[1]).expect("delete");
        assert_eq!(wal.rows().len(), 10, "the WAL has records before the clear");

        engine.clear_wal().expect("clear");
        assert!(wal.rows().is_empty(), "clear_wal must empty the table");
    }

    /// Acceptance: `load_stale_ids` returns exactly the chunk ids whose
    /// flags carry the DEL or UPD bit (ADD rows are not stale).
    #[test]
    fn test_load_stale_ids() {
        let dir = TempDir::new();
        let wal = WalDb::new();
        let engine = UsearchEngine::create(&dir.0, test_config())
            .expect("create")
            .with_wal_db(wal.engine_conn());

        let data = rows(0, 10);
        engine.insert_batch(&batch_refs(&data)).expect("insert");
        assert!(
            engine.load_stale_ids().expect("stale ids").is_empty(),
            "ADD-only rows are not stale"
        );

        engine.delete_by_chunk_ids(&[2]).expect("delete");
        // Seed a UPD record directly: the engine has no update operation in
        // task 2.2, but the stale set must honor the UPD bit (design flags).
        wal.check
            .execute(
                "INSERT OR REPLACE INTO usearch_vectors_log (segment_id, chunk_id, flags, created_at) \
                 VALUES (0, 5, 4, datetime('now'))",
                [],
            )
            .expect("seed UPD record");

        let stale = engine.load_stale_ids().expect("stale ids");
        assert_eq!(
            stale,
            HashSet::from([2u32, 5]),
            "DEL and UPD rows are stale, ADD rows are not"
        );
    }

    /// Acceptance: `rebuild` clears the WAL table (the rebuilt content is
    /// fully persisted to the index file, so the old records are stale).
    #[test]
    fn test_rebuild_clears_wal() {
        let dir = TempDir::new();
        let wal = WalDb::new();
        let engine = UsearchEngine::create(&dir.0, test_config())
            .expect("create")
            .with_wal_db(wal.engine_conn());

        let first = rows(0, 10);
        engine.insert_batch(&batch_refs(&first)).expect("insert");
        engine.delete_by_chunk_ids(&[1]).expect("delete");
        assert_eq!(
            wal.rows().len(),
            10,
            "the WAL has records before the rebuild"
        );

        let second = rows(100, 5);
        engine.rebuild(&second).expect("rebuild");

        assert!(wal.rows().is_empty(), "the rebuild must clear the WAL");
        assert_eq!(
            engine.count().expect("count"),
            5,
            "the rebuild replaced the content"
        );
    }

    // --- Parallel search with WAL filtering (task 2.3) ----------------------

    /// Acceptance: usearch's `filtered_search` excludes stale chunk ids
    /// during HNSW traversal. A deleted chunk (DEL flag in WAL) must not
    /// appear in search results even though its vector is still physically
    /// in the index (the RAM index is mutated, but the test inserts then
    /// journals a DEL without removing from the index to isolate the filter).
    #[test]
    fn test_filtered_search_excludes_stale() {
        let dir = TempDir::new();
        let wal = WalDb::new();
        let engine = UsearchEngine::create(&dir.0, test_config())
            .expect("create")
            .with_wal_db(wal.engine_conn());

        // Insert 10 vectors (ids 0..10).
        let data = rows(0, 10);
        engine.insert_batch(&batch_refs(&data)).expect("insert");

        // Delete chunk 3: this journals a DEL record AND removes from the
        // index. To isolate the filtered_search path (where the vector is
        // still physically present but marked stale), we re-insert chunk 3
        // and then journal only a DEL without removing from the index.
        engine.delete_by_chunk_ids(&[3]).expect("delete");
        engine.insert(3, &data[3].1).expect("re-insert 3");

        // Now chunk 3 is back in the index, but its WAL record is still DEL
        // (the re-insert wrote an ADD, which replaced the DEL). We need to
        // re-journal a DEL to make it stale again.
        engine.delete_by_chunk_ids(&[3]).expect("delete 3 again");

        // At this point chunk 3 is removed from the index AND has a DEL
        // record. Let's verify the search excludes it (it's already gone
        // from the index, so this is a basic sanity check).
        let results = engine.search(&data[3].1, 10).expect("search");
        let ids: Vec<u32> = results.iter().map(|(id, _)| *id).collect();
        assert!(
            !ids.contains(&3),
            "deleted chunk 3 must not appear in results: {ids:?}"
        );

        // Now the key test: insert chunk 3 back (ADD record in WAL),
        // then manually overwrite the WAL record to DEL (simulating a
        // crash between the WAL write and the index mutation).
        engine.insert(3, &data[3].1).expect("re-insert 3");
        // Overwrite the WAL record for chunk 3 to DEL (simulating the
        // WAL-first crash scenario: WAL says DEL but index still has it).
        wal.check
            .execute(
                "INSERT OR REPLACE INTO usearch_vectors_log (segment_id, chunk_id, flags, created_at) \
                 VALUES (0, 3, 2, datetime('now'))",
                [],
            )
            .expect("set chunk 3 to DEL in WAL");

        // Now chunk 3 is in the index but marked stale (DEL) in the WAL.
        // The filtered_search must exclude it.
        let results = engine.search(&data[3].1, 10).expect("search with stale");
        let ids: Vec<u32> = results.iter().map(|(id, _)| *id).collect();
        assert!(
            !ids.contains(&3),
            "stale chunk 3 (DEL in WAL, present in index) must be excluded by filtered_search: {ids:?}"
        );
        // The other vectors are still returned.
        assert!(!results.is_empty(), "other vectors must still be returned");
    }

    /// Acceptance: parallel search returns correct results (top-1 is the
    /// queried vector, results are distance-ascending, correct count).
    #[test]
    fn test_parallel_search_correctness() {
        let dir = TempDir::new();
        let engine = UsearchEngine::create(&dir.0, test_config()).expect("create");

        let data = rows(0, 200);
        engine
            .insert_batch(&batch_refs(&data))
            .expect("insert 200 rows");

        // Query = an exact stored row (a cluster center): top-1 must be
        // that row at ~0 distance.
        let results = engine.search(&data[5].1, 10).expect("search");
        assert_eq!(results.len(), 10, "10 results for 200 stored rows");
        assert_eq!(results[0].0, 5, "top-1 must be the queried row");
        assert!(
            results[0].1 < 1e-3,
            "top-1 distance must be ~0, got {}",
            results[0].1
        );
        // Results are distance-ascending.
        for pair in results.windows(2) {
            assert!(
                pair[0].1 <= pair[1].1,
                "results must be distance-asc: {results:?}"
            );
        }

        // Search with a WAL attached (no stale ids): same results.
        let wal = WalDb::new();
        let dir2 = TempDir::new();
        let engine2 = UsearchEngine::create(&dir2.0, test_config())
            .expect("create")
            .with_wal_db(wal.engine_conn());
        engine2
            .insert_batch(&batch_refs(&data))
            .expect("insert 200 rows");
        let results2 = engine2.search(&data[5].1, 10).expect("search with WAL");
        let keys1: Vec<u32> = results.iter().map(|(id, _)| *id).collect();
        let keys2: Vec<u32> = results2.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            keys1, keys2,
            "WAL with no stale ids must not change results"
        );
    }

    /// Acceptance: `merge_results` deduplicates by chunk_id (keeping the
    /// minimum distance), sorts ascending, and truncates to k.
    #[test]
    fn test_merge_results_deduplication() {
        // Two segments both return chunk_id 5 with different distances:
        // the merge must keep the minimum (1.0 < 2.0).
        let results: Vec<Vec<(u32, f32)>> = vec![
            vec![(5u32, 2.0f32), (3, 1.5), (1, 0.5)],
            vec![(5u32, 1.0f32), (7, 3.0), (1, 0.8)],
        ];
        let merged = merge_results(results, 10);
        // Expected sorted by distance asc: 1 (0.5), 5 (min 1.0), 3 (1.5), 7 (3.0).
        let ids: Vec<u32> = merged.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            vec![1, 5, 3, 7],
            "dedup keeps all unique ids, sorted by distance"
        );
        assert_eq!(merged[0].1, 0.5, "chunk 1 keeps min distance");
        assert_eq!(merged[1].1, 1.0, "chunk 5 keeps min distance (1.0 < 2.0)");

        // Truncation: k=2 keeps only the top-2 by distance.
        let merged = merge_results(
            vec![
                vec![(10u32, 0.1f32), (20, 0.2), (30, 0.3)],
                vec![(40u32, 0.4f32), (50, 0.5)],
            ],
            2,
        );
        assert_eq!(merged.len(), 2, "truncated to k=2");
        assert_eq!(merged[0].0, 10, "top-1 is the closest");
        assert_eq!(merged[1].0, 20, "top-2 is the second closest");

        // Empty input: empty output.
        assert!(merge_results(vec![], 5).is_empty());

        // Single empty segment: empty output.
        assert!(merge_results(vec![vec![]], 5).is_empty());
    }

    /// Acceptance: `stale_vector_percentage` returns the correct percentage.
    #[test]
    fn test_stale_vector_percentage() {
        let dir = TempDir::new();
        let wal = WalDb::new();
        let engine = UsearchEngine::create(&dir.0, test_config())
            .expect("create")
            .with_wal_db(wal.engine_conn());

        // Insert 10 vectors
        let data = rows(0, 10);
        engine.insert_batch(&batch_refs(&data)).expect("insert");

        // No stale vectors yet
        let pct = engine.stale_vector_percentage().expect("pct");
        assert_eq!(pct, 0.0, "no stale vectors");

        // Delete 2 vectors → 2 stale / 8 live = 25%
        // (index has 8 vectors, WAL has 2 DEL records)
        engine.delete_by_chunk_ids(&[0, 1]).expect("delete");
        let pct = engine.stale_vector_percentage().expect("pct");
        assert!((pct - 25.0).abs() < 0.01, "25% stale, got {pct}");
    }

    /// Acceptance: `maybe_compact` triggers when stale > threshold.
    #[test]
    fn test_compaction_triggers_on_threshold() {
        let dir = TempDir::new();
        let wal = WalDb::new();
        let mut config = test_config();
        config.usearch = Some(crate::UsearchConfig {
            max_segment_vectors: 1_000_000,
            compaction_stale_threshold: 30,
            search_threads: 4,
        });
        let mut engine = UsearchEngine::create_with_config(
            &dir.0,
            config.clone(),
            config.usearch.clone().unwrap(),
        )
        .expect("create")
        .with_wal_db(wal.engine_conn());

        // Insert 10 vectors
        let data = rows(0, 10);
        engine.insert_batch(&batch_refs(&data)).expect("insert");

        // Delete 2 (20%) — below threshold, no compaction
        engine.delete_by_chunk_ids(&[0, 1]).expect("delete");
        engine.maybe_compact().expect("maybe_compact");
        // WAL should still have records (no compaction happened)
        let stale = engine.load_all_stale_ids().expect("stale");
        assert_eq!(stale.len(), 2, "no compaction at 20%");

        // Delete 2 more (40%) — above threshold, compaction triggers
        engine.delete_by_chunk_ids(&[2, 3]).expect("delete");
        engine.maybe_compact().expect("maybe_compact");
        // WAL should be cleared after compaction
        let stale = engine.load_all_stale_ids().expect("stale after compact");
        assert!(stale.is_empty(), "WAL cleared after compaction");
    }

    /// Acceptance: `compact` creates correct segment count.
    #[test]
    fn test_compaction_creates_correct_segments() {
        let dir = TempDir::new();
        let wal = WalDb::new();
        let mut config = test_config();
        config.usearch = Some(crate::UsearchConfig {
            max_segment_vectors: 5, // Small for testing
            compaction_stale_threshold: 30,
            search_threads: 4,
        });
        let mut engine = UsearchEngine::create_with_config(
            &dir.0,
            config.clone(),
            config.usearch.clone().unwrap(),
        )
        .expect("create")
        .with_wal_db(wal.engine_conn());

        // Insert 12 vectors
        let data = rows(0, 12);
        engine.insert_batch(&batch_refs(&data)).expect("insert");

        // Manually trigger compaction
        engine.compact().expect("compact");

        // 12 vectors / 5 per segment = 3 segments
        assert_eq!(
            engine.disk_segments.len(),
            3,
            "12 vectors / 5 per segment = 3 segments"
        );
    }

    /// Acceptance: `compact` clears the WAL table.
    #[test]
    fn test_compaction_clears_wal() {
        let dir = TempDir::new();
        let wal = WalDb::new();
        let mut engine = UsearchEngine::create(&dir.0, test_config())
            .expect("create")
            .with_wal_db(wal.engine_conn());

        // Insert and delete to create WAL records
        let data = rows(0, 10);
        engine.insert_batch(&batch_refs(&data)).expect("insert");
        engine.delete_by_chunk_ids(&[0, 1, 2]).expect("delete");

        // WAL has records
        let stale = engine.load_all_stale_ids().expect("stale before");
        assert_eq!(stale.len(), 3, "3 stale records");

        // Compact
        engine.compact().expect("compact");

        // WAL cleared
        let stale = engine.load_all_stale_ids().expect("stale after");
        assert!(stale.is_empty(), "WAL cleared after compact");
    }

    /// Acceptance: search returns correct results after compaction.
    #[test]
    fn test_search_after_compaction() {
        let dir = TempDir::new();
        let wal = WalDb::new();
        let mut engine = UsearchEngine::create(&dir.0, test_config())
            .expect("create")
            .with_wal_db(wal.engine_conn());

        // Insert 10 vectors
        let data = rows(0, 10);
        engine.insert_batch(&batch_refs(&data)).expect("insert");

        // Search before compaction
        let before = engine.search(&data[5].1, 5).expect("search before");
        let before_ids: Vec<u32> = before.iter().map(|(id, _)| *id).collect();
        assert!(before_ids.contains(&5), "chunk 5 found before compaction");

        // Compact
        engine.compact().expect("compact");

        // Search after compaction — same results
        let after = engine.search(&data[5].1, 5).expect("search after");
        let after_ids: Vec<u32> = after.iter().map(|(id, _)| *id).collect();
        assert!(after_ids.contains(&5), "chunk 5 found after compaction");
        assert_eq!(
            before.len(),
            after.len(),
            "result count unchanged after compaction"
        );
    }
}
