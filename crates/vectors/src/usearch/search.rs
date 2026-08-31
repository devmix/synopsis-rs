//! Search-path helpers (usearch-wal-persistence task 3.4 split; parallel
//! search per ADR 0004 §6, task 3.6): the multi-layer `search`
//! implementation on the dedicated `search_threads` pool, concurrent row
//! insertion, and the freshest-wins per-layer result merge.
//!
//! The stale filtering reads the versioned in-memory stale-set cache
//! (ADR 0004 §3/§6) — zero SQL on the query path. Each layer filters by its
//! OWN per-segment stale set (a key superseded in an older segment stays
//! live in the fresher layer, so a global stale set would wrongly hide it).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rayon::prelude::*;
use usearch::Index;

use super::options::{key_to_chunk_id, map_usearch};
use super::{UsearchEngine, disk_segments_read_guard};
use crate::VectorsError;

/// Rows per rayon task in `insert_batch`/`rebuild` (mirrors the
/// LanceEngine batch size of 1000; each row is a single concurrent `add`).
pub(super) const ADD_CHUNK: usize = 1000;

/// The RAM layer's freshness rank in the merge (ADR 0004 §2): the RAM
/// layer (segment id 0) is the freshest of all layers, above every DISK
/// id — a higher rank wins a duplicate chunk id.
const RAM_RANK: u32 = u32::MAX;

/// Builds the engine's dedicated search pool sized by
/// `UsearchConfig::search_threads` (ADR 0004 §6/§9: the superseded
/// implementation ignored the config — defect #9). `threads` is validated
/// `> 0` by `UsearchConfig::validate` before this is called.
pub(super) fn build_search_pool(threads: usize) -> Result<rayon::ThreadPool, VectorsError> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(|err| VectorsError::Engine(format!("search thread pool: {err}")))
}

impl UsearchEngine {
    /// Top-k nearest neighbours of `query`: `(chunk_id, distance)` pairs
    /// sorted by distance ascending (L2sq on the quantized storage — see the
    /// module docs). An empty index yields an empty vec (not an error).
    /// Requires `k > 0` and `query.len() == config.dim`.
    ///
    /// ADR 0004 §6: the search runs across ALL layers — the RAM layer plus
    /// every DISK segment — in parallel on the engine's dedicated
    /// `search_threads`-sized rayon pool (built in `create_with_config` /
    /// `open_with_wal`), not the global pool. Each layer returns its own
    /// top-k after stale filtering: a layer whose stale set is empty uses
    /// plain `search` (no filter-closure overhead), the rest use
    /// `filtered_search` (the filter is evaluated per-candidate inside the
    /// C++ core, not as a post-filter). The stale sets come from the
    /// versioned in-memory cache — zero SQL on the query path. A key
    /// superseded in an older segment stays live in the fresher layer,
    /// which is why the filter is per-segment, not global.
    ///
    /// The merge (ADR 0004 §2/§6 step 4) resolves a duplicate chunk id in
    /// favour of the FRESHEST layer (RAM > higher segment id): the fresh
    /// layer holds the current vector, so its distance — not the minimum
    /// distance — is reported. Duplicates exist only in crash windows (the
    /// write path supersedes on insert), so this is the read-path
    /// protection. The final order is distance ascending (id tiebreak);
    /// the length is ≤ `k`.
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u32, f32)>, VectorsError> {
        self.config.validate_search(query, k)?;

        // The per-segment stale sets come from the versioned in-memory
        // cache (ADR 0004 §3): zero SQL on the query path.
        let stale = self.stale.snapshot();

        // ADR 0004 §6 step 2: clone the current DISK segment Arcs under a
        // short read lock (rebuild/compaction replace the list; searches
        // keep the old views alive).
        let segments: Vec<(u32, Arc<Index>)> = {
            let guard = disk_segments_read_guard(&self.disk_segments);
            guard
                .iter()
                .map(|segment| (segment.id, segment.index.clone()))
                .collect()
        };

        // ADR 0004 §6 step 3: search [RAM] + DISK[n] in parallel on the
        // dedicated pool. `install` returns the closure's value (a
        // `Result`), so the error propagates through `?` below.
        let layers: Result<Vec<LayerResults>, VectorsError> = self.search_pool.install(|| {
            let disk: Vec<LayerResults> = segments
                .par_iter()
                .map(|(id, index)| {
                    let results = search_layer(index, query, k, stale.get(id))?;
                    Ok(LayerResults { rank: *id, results })
                })
                .collect::<Result<Vec<_>, VectorsError>>()?;
            let mut layers = disk;
            // The RAM layer (segment 0) is the freshest layer.
            layers.push(LayerResults {
                rank: RAM_RANK,
                results: search_layer(&self.index, query, k, stale.get(&0))?,
            });
            Ok(layers)
        });

        // ADR 0004 §6 step 4: freshest-wins merge, distance ascending, ≤ k.
        Ok(merge_results(layers?, k))
    }
}

/// One layer's search result: the layer's freshness rank (higher = fresher:
/// [`RAM_RANK`] for the RAM layer, the segment id for a DISK layer) and its
/// top-k candidates.
struct LayerResults {
    rank: u32,
    results: Vec<(u32, f32)>,
}

/// One layer's top-k after stale filtering (ADR 0004 §6 step 3): plain
/// `search` when the layer's stale set is empty (no filter-closure
/// overhead), `filtered_search` otherwise (evaluated per-candidate inside
/// the C++ core, not a post-filter).
fn search_layer(
    index: &Index,
    query: &[f32],
    k: usize,
    stale: Option<&HashSet<u32>>,
) -> Result<Vec<(u32, f32)>, VectorsError> {
    let matches = match stale {
        Some(set) if !set.is_empty() => index
            .filtered_search(query, k, |key: u64| !set.contains(&(key as u32)))
            .map_err(map_usearch)?,
        _ => index.search(query, k).map_err(map_usearch)?,
    };
    to_results(&matches.keys, &matches.distances)
}

/// Adds all rows to a concurrent index, chunked into rayon tasks of
/// [`ADD_CHUNK`] (the rows must already be dim-validated by the caller).
pub(super) fn add_rows(index: &Index, rows: &[(u32, &[f32])]) -> Result<(), VectorsError> {
    rows.par_chunks(ADD_CHUNK)
        .try_for_each(|chunk| {
            for &(chunk_id, vector) in chunk {
                index.add(chunk_id as u64, vector).map_err(map_usearch)?;
            }
            Ok(())
        })
        .map(|_| ())
}

/// Converts usearch search `matches` (keys + distances) into
/// `(chunk_id, distance)` pairs.
fn to_results(keys: &[u64], distances: &[f32]) -> Result<Vec<(u32, f32)>, VectorsError> {
    let mut results = Vec::with_capacity(keys.len());
    for j in 0..keys.len() {
        results.push((key_to_chunk_id(keys[j])?, distances[j]));
    }
    Ok(results)
}

/// Merges the per-layer search results (ADR 0004 §6 step 4, task 3.6): a
/// duplicate chunk id is resolved in favour of the FRESHEST layer (higher
/// rank: RAM > higher segment id — the fresh layer holds the current
/// vector, so its distance wins over the older layers'), then the
/// survivors are sorted by distance ascending (id tiebreak) and truncated
/// to `k`.
fn merge_results(layers: Vec<LayerResults>, k: usize) -> Vec<(u32, f32)> {
    // chunk_id → (distance, freshness rank).
    let mut merged: HashMap<u32, (f32, u32)> = HashMap::new();
    for layer in layers {
        for (id, distance) in layer.results {
            let entry = merged.entry(id).or_insert((distance, layer.rank));
            // A strictly fresher layer supersedes the stored entry;
            // within one layer ids are unique (non-multi index).
            if layer.rank > entry.1 {
                *entry = (distance, layer.rank);
            }
        }
    }
    let mut sorted: Vec<_> = merged
        .into_iter()
        .map(|(id, (distance, _))| (id, distance))
        .collect();
    sorted.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    sorted.truncate(k);
    sorted
}

#[cfg(test)]
mod search_tests {
    //! Parallel search tests (usearch-wal-persistence task 3.6, ADR 0004
    //! §6): the dedicated `search_threads` pool, per-layer stale
    //! filtering, and the freshest-wins merge.

    // Test code: unwrap/expect are intentional (the fixtures are deterministic).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use rusqlite::Connection;

    use super::super::UsearchEngine;
    use super::super::test_util::{
        TempDir, create_wal_table, test_config, test_vector, write_segment,
    };
    use crate::UsearchConfig;

    /// A WAL database (table created) at `dir/knowledge.db`.
    fn wal_db(dir: &TempDir) -> std::path::PathBuf {
        let path = dir.0.join("knowledge.db");
        let conn = Connection::open(&path).unwrap();
        create_wal_table(&conn);
        drop(conn);
        path
    }

    /// ADR 0004 §9 (defect #9 closed): the dedicated pool is sized by
    /// `UsearchConfig::search_threads` — not the global pool, not the
    /// config default.
    #[test]
    fn search_pool_is_sized_by_config() {
        let dir = TempDir::new("pool-size");
        let usearch_config = UsearchConfig {
            search_threads: 2,
            ..UsearchConfig::default()
        };
        let engine =
            UsearchEngine::create_with_config(&dir.0, test_config(), usearch_config).unwrap();
        assert_eq!(
            engine.search_pool.current_num_threads(),
            2,
            "the pool must use the configured thread count"
        );
    }

    /// The config default (`search_threads = 4`) sizes the pool when no
    /// explicit `UsearchConfig` is passed (`create`).
    #[test]
    fn search_pool_defaults_to_config_default() {
        let dir = TempDir::new("pool-default");
        let engine = UsearchEngine::create(&dir.0, test_config()).unwrap();
        assert_eq!(
            engine.search_pool.current_num_threads(),
            UsearchConfig::default().search_threads,
            "the default config must size the pool"
        );
    }

    /// Regression: a RAM-only search is unchanged by the parallel rewrite —
    /// top-k by distance ascending, k truncation, empty index → empty vec.
    #[test]
    fn ram_only_search_unchanged() {
        let dir = TempDir::new("ram-only");
        let config = test_config();
        let engine = UsearchEngine::create(&dir.0, config.clone()).unwrap();
        for axis in 1..=5usize {
            engine.insert(axis as u32, &test_vector(8, axis)).unwrap();
        }

        // Query = axis-3 vector: the exact match first (distance 0), the
        // four others at L2sq 2.0.
        let results = engine.search(&test_vector(8, 3), 5).unwrap();
        assert_eq!(results.len(), 5);
        assert_eq!(results[0].0, 3, "the exact match ranks first: {results:?}");
        assert!(results[0].1.abs() < f32::EPSILON);
        for pair in results.windows(2) {
            assert!(pair[0].1 <= pair[1].1, "distance ascending: {results:?}");
        }

        // k truncation: more candidates than k → exactly k, sorted.
        let top3 = engine.search(&test_vector(8, 3), 3).unwrap();
        assert_eq!(top3.len(), 3, "truncated to k: {top3:?}");
        for pair in top3.windows(2) {
            assert!(pair[0].1 <= pair[1].1, "distance ascending: {top3:?}");
        }

        // Empty index → empty vec, not an error.
        let empty = UsearchEngine::create(&TempDir::new("ram-empty").0, config).unwrap();
        assert!(empty.search(&test_vector(8, 1), 3).unwrap().is_empty());
    }

    /// ADR 0004 §6: RAM + a pre-seeded DISK segment — the query returns the
    /// union, and a key present in BOTH layers (stale row in DISK, live in
    /// RAM) appears ONCE with the RAM vector's distance (the stale DISK
    /// copy never surfaces).
    #[test]
    fn ram_supersedes_disk_key_in_search() {
        let dir = TempDir::new("ram-supersede");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();
        // segment-1: key 7 → axis-1 vector, key 8 → axis-2 vector.
        write_segment(&dir.0, 1, &config, &[7, 8]);
        let db_path = wal_db(&dir);
        let engine = UsearchEngine::open_with_wal(&dir.0, config, Some(&db_path)).unwrap();

        // Re-insert 7 in RAM (axis-3 vector): the WAL gets (1, 7, DEL).
        engine.insert(7, &test_vector(8, 3)).unwrap();

        // Query = axis-3 vector: 7 (RAM, distance 0) + 8 (DISK, distance 2).
        let results = engine.search(&test_vector(8, 3), 5).unwrap();
        assert_eq!(
            results,
            vec![(7, 0.0), (8, 2.0)],
            "7 must appear once with the RAM distance: {results:?}"
        );
    }

    /// ADR 0004 §6: a key stale in BOTH layers (deleted) does not appear
    /// in the results even though the index files still contain it (DISK
    /// segments are never mutated; the RAM `remove` is in-memory).
    #[test]
    fn deleted_key_absent_from_all_layers() {
        let dir = TempDir::new("deleted-both");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();
        write_segment(&dir.0, 1, &config, &[7, 8]);
        let db_path = wal_db(&dir);
        let engine = UsearchEngine::open_with_wal(&dir.0, config, Some(&db_path)).unwrap();

        // 7 lives in both layers after the re-insert; delete it: the WAL
        // gets (0, 7, DEL) + (1, 7, DEL), RAM removes it.
        engine.insert(7, &test_vector(8, 3)).unwrap();
        engine.delete_by_chunk_ids(&[7]).unwrap();

        // The stale sets hide 7 in every layer (query = the RAM vector).
        let results = engine.search(&test_vector(8, 3), 5).unwrap();
        assert!(
            !results.iter().any(|(id, _)| *id == 7),
            "the deleted key must not appear: {results:?}"
        );
        // 8 is still live in segment-1 (query = its axis-2 vector).
        let results = engine.search(&test_vector(8, 2), 5).unwrap();
        assert_eq!(results.first().map(|(id, _)| *id), Some(8));
    }

    /// ADR 0004 §2: a duplicate chunk id across DISK layers resolves to the
    /// FRESHER segment (higher id) — NOT the minimum distance. Duplicates
    /// exist only in crash windows (the write path supersedes), so this is
    /// the read-path protection.
    #[test]
    fn fresher_disk_segment_wins_the_merge() {
        let dir = TempDir::new("fresher-segment");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();
        // Crash-window state (no WAL): key 5 in TWO segments with
        // different vectors — segment-1: axis 1, segment-2: axis 2.
        write_segment(&dir.0, 1, &config, &[5]); // 5 → axis 1
        write_segment(&dir.0, 2, &config, &[9, 5]); // 9 → axis 1, 5 → axis 2
        let engine = UsearchEngine::open(&dir.0, config).unwrap();

        // Query = axis-1 vector: the OLDER segment-1 copy of 5 is the
        // closer one (distance 0); the fresher segment-2 copy is at 2.
        let results = engine.search(&test_vector(8, 1), 5).unwrap();
        let five = results
            .iter()
            .find(|(id, _)| *id == 5)
            .expect("key 5 must be present");
        assert_eq!(
            five.1, 2.0,
            "the fresher segment's distance must win (not the minimum): {results:?}"
        );
        assert_eq!(
            results.iter().filter(|(id, _)| *id == 5).count(),
            1,
            "5 must appear exactly once: {results:?}"
        );
    }

    /// ADR 0004 §2: RAM beats any DISK layer — even when the DISK copy is
    /// the closer one, the RAM vector's distance is reported.
    #[test]
    fn ram_distance_wins_over_closer_disk_copy() {
        let dir = TempDir::new("ram-distance-wins");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();
        write_segment(&dir.0, 1, &config, &[7]); // 7 → axis 1
        let engine = UsearchEngine::open(&dir.0, config).unwrap();
        // No WAL attached: the re-insert writes no supersession row, so 7
        // is live in BOTH layers (crash-window state). RAM copy: axis 3.
        engine.insert(7, &test_vector(8, 3)).unwrap();

        // Query = axis-1 vector: the DISK copy is closer (0.0), the RAM
        // copy is at 2.0 — the freshest layer (RAM) must win.
        let results = engine.search(&test_vector(8, 1), 5).unwrap();
        let seven = results
            .iter()
            .find(|(id, _)| *id == 7)
            .expect("key 7 must be present");
        assert_eq!(
            seven.1, 2.0,
            "the RAM copy's distance must win (not the minimum): {results:?}"
        );
    }

    /// ADR 0004 §6 step 4 across layers: with more candidates than k (RAM +
    /// DISK), the merged result has length exactly k, sorted by distance
    /// ascending.
    #[test]
    fn merge_truncates_to_k_sorted_by_distance() {
        let dir = TempDir::new("k-truncate");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();
        write_segment(&dir.0, 1, &config, &[1, 2, 3]); // axes 1..3
        let engine = UsearchEngine::open(&dir.0, config.clone()).unwrap();
        engine
            .insert_batch(&[(4, &test_vector(8, 4)), (5, &test_vector(8, 5))])
            .unwrap();

        // 5 candidates (3 DISK + 2 RAM), k = 3 → exactly 3, distance asc.
        let results = engine.search(&test_vector(8, 4), 3).unwrap();
        assert_eq!(results.len(), 3, "truncated to k: {results:?}");
        for pair in results.windows(2) {
            assert!(pair[0].1 <= pair[1].1, "distance ascending: {results:?}");
        }
        assert_eq!(results[0].0, 4, "the exact match ranks first: {results:?}");
    }
}
