//! Search-path helpers (usearch-wal-persistence task 3.4 split): the
//! multi-segment `search` implementation, concurrent row insertion, and
//! the per-segment result merge.
//!
//! The stale filtering reads the versioned in-memory stale-set cache
//! (ADR 0004 §3/§6) — zero SQL on the query path. Each layer filters by its
//! OWN per-segment stale set (a key superseded in an older segment stays
//! live in the fresher layer, so a global stale set would wrongly hide it).

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
    /// Stale filtering (ADR 0004 §3/§6): each layer excludes the keys in its
    /// OWN per-segment stale set, evaluated per-candidate inside the C++ core
    /// via usearch's `filtered_search` (not a post-filter), so deleted or
    /// superseded vectors never appear in the results. The stale sets come
    /// from the versioned in-memory cache — zero SQL on the query path. A
    /// layer with an empty stale set uses plain `search` (no filter-closure
    /// overhead). A key superseded in an older segment stays live in the
    /// fresher layer, which is why the filter is per-segment, not global.
    ///
    /// The search runs across the engine's segments in parallel (rayon
    /// `par_iter`); per-segment results are merged by [`merge_results`]
    /// (dedup by chunk_id keeping the minimum distance, sort ascending,
    /// truncate to `k`). The freshest-wins merge and the dedicated
    /// `search_threads` pool land in task 3.6.
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u32, f32)>, VectorsError> {
        self.config.validate_search(query, k)?;

        // The per-segment stale sets come from the versioned in-memory cache
        // (ADR 0004 §3): zero SQL on the query path.
        let stale = self.stale.snapshot();

        // Search the DISK segments in parallel; each filters by its OWN
        // stale set only (ADR 0004 §3: a segment is self-contained).
        let segments = disk_segments_read_guard(&self.disk_segments);
        let disk_results: Vec<Vec<(u32, f32)>> = segments
            .par_iter()
            .map(|segment| {
                let matches = match stale.get(&segment.id) {
                    Some(seg_stale) if !seg_stale.is_empty() => segment
                        .index
                        .filtered_search(query, k, |key: u64| !seg_stale.contains(&(key as u32)))
                        .map_err(map_usearch)?,
                    _ => segment.index.search(query, k).map_err(map_usearch)?,
                };
                to_results(&matches.keys, &matches.distances)
            })
            .collect::<Result<Vec<_>, VectorsError>>()?;
        drop(segments);

        // Search the RAM layer (segment 0) with its own stale set.
        let ram_matches = match stale.get(&0) {
            Some(ram_stale) if !ram_stale.is_empty() => self
                .index
                .filtered_search(query, k, |key: u64| !ram_stale.contains(&(key as u32)))
                .map_err(map_usearch)?,
            _ => self.index.search(query, k).map_err(map_usearch)?,
        };
        let ram_vec = to_results(&ram_matches.keys, &ram_matches.distances)?;

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

/// Converts usearch search `matches` (keys + distances) into
/// `(chunk_id, distance)` pairs.
fn to_results(keys: &[u64], distances: &[f32]) -> Result<Vec<(u32, f32)>, VectorsError> {
    let mut results = Vec::with_capacity(keys.len());
    for j in 0..keys.len() {
        results.push((key_to_chunk_id(keys[j])?, distances[j]));
    }
    Ok(results)
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
