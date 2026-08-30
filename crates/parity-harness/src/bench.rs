//! A/B engine benchmark (add-usearch-ann-engine, task 1.4).
//!
//! Measures recall@k, p50/p95 search latency, and on-disk index size for any
//! [`VectorIndex`] implementation on a common set of queries. The caller
//! provides the pre-built engine, the queries, the ground truth, the measured
//! build time, and the index directory for size measurement.

use std::path::Path;
use std::time::Instant;

use vectors::{VectorIndex, VectorsError};

/// Result of an A/B benchmark run for one engine.
#[derive(Debug, Clone)]
pub struct EngineBenchResult {
    /// Mean recall@k against the provided ground truth.
    pub recall_at_k: f64,
    /// p50 search latency in microseconds.
    pub p50_us: f64,
    /// p95 search latency in microseconds.
    pub p95_us: f64,
    /// Total build time (create + insert + build_index) in milliseconds.
    pub build_time_ms: u64,
    /// Total on-disk index size in bytes.
    pub index_size_bytes: u64,
}

/// Benchmarks a pre-built vector index engine.
///
/// Runs each query against the engine, measures per-query latency, computes
/// recall@k against `ground_truth`, and walks `index_dir` to measure the
/// on-disk footprint. `build_time_ms` is passed in by the caller (who timed
/// the create + insert + build_index sequence).
///
/// Returns an error if any search fails or the query/ground-truth counts
/// mismatch.
pub fn bench_engine(
    engine: &dyn VectorIndex,
    queries: &[Vec<f32>],
    k: usize,
    ground_truth: &[Vec<u32>],
    build_time_ms: u64,
    index_dir: &Path,
) -> Result<EngineBenchResult, VectorsError> {
    let mut candidates: Vec<Vec<u32>> = Vec::with_capacity(queries.len());
    let mut latencies_us: Vec<f64> = Vec::with_capacity(queries.len());

    for query in queries {
        let start = Instant::now();
        let results = engine.search(query, k)?;
        let elapsed_us = start.elapsed().as_secs_f64() * 1_000_000.0;
        latencies_us.push(elapsed_us);
        candidates.push(results.into_iter().map(|(id, _)| id).collect());
    }

    let recall = crate::metrics::recall_at_k(&candidates, ground_truth).ok_or_else(|| {
        VectorsError::InvalidArgument(
            "bench_engine: query count must match ground truth count".to_string(),
        )
    })?;

    let p50 = percentile(&latencies_us, 50);
    let p95 = percentile(&latencies_us, 95);
    let index_size = dir_size_bytes(index_dir);

    Ok(EngineBenchResult {
        recall_at_k: recall,
        p50_us: p50,
        p95_us: p95,
        build_time_ms,
        index_size_bytes: index_size,
    })
}

/// Nearest-rank percentile of float values (p in 0..=100).
///
/// Returns 0.0 for empty input.
fn percentile(values: &[f64], p: u32) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let rank = ((p as f64 / 100.0) * sorted.len() as f64).ceil() as usize;
    let idx = rank.clamp(1, sorted.len());
    sorted[idx - 1]
}

/// Recursively sums the file sizes under `dir`.
///
/// Returns 0 if the directory does not exist or is unreadable.
pub fn dir_size_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                total += dir_size_bytes(&path);
            } else if let Ok(meta) = path.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn percentile_empty_returns_zero() {
        assert_eq!(percentile(&[], 50), 0.0);
        assert_eq!(percentile(&[], 95), 0.0);
    }

    #[test]
    fn percentile_single_value() {
        assert_eq!(percentile(&[42.0], 50), 42.0);
        assert_eq!(percentile(&[42.0], 95), 42.0);
    }

    #[test]
    fn percentile_known_dataset() {
        let values: Vec<f64> = (1..=10).map(|v| v as f64).collect();
        // rank = ceil(p/100 * n): p50 -> ceil(5.0) = 5th element = 5.0
        // p95 -> ceil(9.5) = 10th element = 10.0
        assert_eq!(percentile(&values, 50), 5.0);
        assert_eq!(percentile(&values, 95), 10.0);
    }

    #[test]
    fn percentile_unsorted_input() {
        let values = vec![9.0, 1.0, 5.0];
        // sorted: [1.0, 5.0, 9.0]; p50 -> ceil(1.5) = 2nd = 5.0
        assert_eq!(percentile(&values, 50), 5.0);
    }

    #[test]
    fn dir_size_bytes_missing_dir_is_zero() {
        assert_eq!(dir_size_bytes(Path::new("/nonexistent-path-xyz")), 0);
    }
}
