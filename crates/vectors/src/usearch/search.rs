//! Search-path helpers (usearch-wal-persistence task 3.4 split): the
//! multi-segment `search` implementation, concurrent row insertion, and
//! the per-segment result merge.

use std::collections::HashSet;

use rayon::prelude::*;
use usearch::Index;

use super::options::{key_to_chunk_id, map_usearch};
use super::{UsearchEngine, disk_segments_read_guard};
use crate::VectorsError;

/// Rows per rayon task in `insert_batch`/`rebuild` (mirrors the
/// LanceEngine batch size of 1000; each row is a single concurrent `add`).
pub(super) const ADD_CHUNK: usize = 1000;

impl UsearchEngine {
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
