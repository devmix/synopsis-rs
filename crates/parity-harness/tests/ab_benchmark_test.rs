//! A/B engine benchmark test (add-usearch-ann-engine, task 1.4).
//!
//! Builds the same index with both LanceEngine and UsearchEngine (when both
//! features are enabled), runs identical queries, and compares recall@k +
//! p50/p95 latency. Also measures build time and index size delta.
//!
//! Gates (design.md "Критерии решения"):
//! - Lance recall@10 >= 0.95
//! - Usearch recall@k within 2% of Lance (recall_usearch >= recall_lance * 0.98)
//! - Usearch p50 <= Lance p50 * 1.2
//! - Usearch p95 <= Lance p95 * 1.3
//!
//! Skips gracefully if `fixtures/vectors.bin` does not exist (file not
//! committed).
//!
//! The test compiles under `engine-lance` only, `engine-usearch` only, and
//! both. The full A/B comparison fires only when both engines are available.

// Test code: unwrap/expect are intentional (test-infra failures panic with
// a message rather than plumbing Results through helpers). Some imports and
// variables are only used when both engine features are enabled.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_imports,
    unused_variables
)]

use std::path::PathBuf;
use std::time::Instant;

use parity_harness::bench::{EngineBenchResult, bench_engine};
use parity_harness::fixtures::load_fixture_set_from_dir;
use vectors::VectorIndexConfig;

/// Top-k for the benchmark.
const K: usize = 10;
/// Stride between query vectors in the fixture (270 rows / stride >= 21).
const QUERY_STRIDE: usize = 13;
/// Number of benchmark queries (>= 20 per task requirement).
const QUERY_COUNT: usize = 21;
/// Lance recall@10 gate (design.md). Only used when both engines are available.
#[cfg(all(feature = "engine-lance", feature = "engine-usearch"))]
const RECALL_GATE: f64 = 0.95;
/// Usearch recall must be within 2% of Lance (design.md).
#[cfg(all(feature = "engine-lance", feature = "engine-usearch"))]
const RECALL_RATIO: f64 = 0.98;
/// Usearch p50 must be within 20% of Lance (design.md).
#[cfg(all(feature = "engine-lance", feature = "engine-usearch"))]
const P50_RATIO: f64 = 1.2;
/// Usearch p95 must be within 30% of Lance (design.md).
#[cfg(all(feature = "engine-lance", feature = "engine-usearch"))]
const P95_RATIO: f64 = 1.3;

/// A/B benchmark: build both engines on the same fixture, run identical
/// queries, and compare recall@k + p50/p95 latency.
#[test]
fn ab_benchmark() {
    let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    if !fixture_dir.join("vectors.bin").is_file() {
        eprintln!(
            "SKIP: fixtures/vectors.bin not found (not committed); \
             run the fixture export from the Go oracle first"
        );
        return;
    }

    // 1. Load the committed SYNX fixture (dim=384, 270 rows).
    let rows = load_fixture_set_from_dir(&fixture_dir).expect("committed fixture loads");
    assert!(!rows.is_empty(), "fixture must have rows");
    let dim = rows[0].1.len();
    assert!(
        rows.iter().all(|(_, v)| v.len() == dim),
        "all fixture rows must have the same dim"
    );

    // 2. Select queries: 21 fixture vectors spread by stride.
    let queries: Vec<Vec<f32>> = rows
        .iter()
        .step_by(QUERY_STRIDE)
        .take(QUERY_COUNT)
        .map(|(_, v)| v.clone())
        .collect();
    assert_eq!(queries.len(), QUERY_COUNT, "query count");

    // 3. Ground truth: exact squared-L2 top-K per query (f64 accumulation).
    let ground_truth: Vec<Vec<u32>> = queries
        .iter()
        .map(|query| {
            let mut dists: Vec<(f64, u32)> = rows
                .iter()
                .map(|(id, vector)| {
                    let acc = query
                        .iter()
                        .zip(vector)
                        .map(|(a, b)| {
                            let d = *a as f64 - *b as f64;
                            d * d
                        })
                        .sum();
                    (acc, *id)
                })
                .collect();
            dists.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
            dists.into_iter().take(K).map(|(_, id)| id).collect()
        })
        .collect();

    // 4. Build and benchmark each available engine.
    let scratch = std::env::temp_dir().join(format!(
        "synopsis-ab-bench-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&scratch).expect("create scratch dir");

    let lance_result: Option<EngineBenchResult> = {
        #[cfg(feature = "engine-lance")]
        {
            Some(build_and_bench_lance(
                &rows,
                dim,
                &queries,
                &ground_truth,
                &scratch,
            ))
        }
        #[cfg(not(feature = "engine-lance"))]
        {
            None
        }
    };

    let usearch_result: Option<EngineBenchResult> = {
        #[cfg(feature = "engine-usearch")]
        {
            Some(build_and_bench_usearch(
                &rows,
                dim,
                &queries,
                &ground_truth,
                &scratch,
            ))
        }
        #[cfg(not(feature = "engine-usearch"))]
        {
            None
        }
    };

    // 5. Print comparison table.
    print_comparison_table(dim, lance_result.as_ref(), usearch_result.as_ref());

    // 6. Assert gates (only when both engines are available).
    #[cfg(all(feature = "engine-lance", feature = "engine-usearch"))]
    {
        let lance = lance_result.expect("lance engine must be available");
        let usearch = usearch_result.expect("usearch engine must be available");

        assert!(
            lance.recall_at_k >= RECALL_GATE,
            "Lance recall@{K} = {} is below the {} gate",
            lance.recall_at_k,
            RECALL_GATE
        );
        assert!(
            usearch.recall_at_k >= lance.recall_at_k * RECALL_RATIO,
            "Usearch recall@{K} = {} is more than 2% below Lance recall@{K} = {} \
             (floor {})",
            usearch.recall_at_k,
            lance.recall_at_k,
            lance.recall_at_k * RECALL_RATIO
        );
        assert!(
            usearch.p50_us <= lance.p50_us * P50_RATIO,
            "Usearch p50 = {} us exceeds {}x Lance p50 = {} us (gate {} us)",
            usearch.p50_us,
            P50_RATIO,
            lance.p50_us,
            lance.p50_us * P50_RATIO
        );
        assert!(
            usearch.p95_us <= lance.p95_us * P95_RATIO,
            "Usearch p95 = {} us exceeds {}x Lance p95 = {} us (gate {} us)",
            usearch.p95_us,
            P95_RATIO,
            lance.p95_us,
            lance.p95_us * P95_RATIO
        );
    }

    let _ = std::fs::remove_dir_all(&scratch);
}

/// Builds a LanceEngine over the fixture rows, times the build, benchmarks
/// search, and returns the result.
#[cfg(feature = "engine-lance")]
fn build_and_bench_lance(
    rows: &[(u32, Vec<f32>)],
    dim: usize,
    queries: &[Vec<f32>],
    ground_truth: &[Vec<u32>],
    scratch: &std::path::Path,
) -> EngineBenchResult {
    let index_dir = scratch.join("lance");
    let config = VectorIndexConfig::new(dim, 16, 100, 8, 8, 200).expect("config valid");

    let build_start = Instant::now();
    let engine = vectors::LanceEngine::create(&index_dir, config).expect("lance create");
    let refs: Vec<(u32, &[f32])> = rows.iter().map(|(id, v)| (*id, v.as_slice())).collect();
    engine.insert_batch(&refs).expect("lance insert");
    engine.build_index().expect("lance build_index");
    let build_time_ms = build_start.elapsed().as_millis() as u64;

    bench_engine(&engine, queries, K, ground_truth, build_time_ms, &index_dir).expect("lance bench")
}

/// Builds a UsearchEngine over the fixture rows, times the build, benchmarks
/// search, and returns the result.
#[cfg(feature = "engine-usearch")]
fn build_and_bench_usearch(
    rows: &[(u32, Vec<f32>)],
    dim: usize,
    queries: &[Vec<f32>],
    ground_truth: &[Vec<u32>],
    scratch: &std::path::Path,
) -> EngineBenchResult {
    let index_dir = scratch.join("usearch");
    // The benchmark runs the usearch engine at its default quantization
    // (bf16), matching the configured default; the recall/latency gates are
    // evaluated against that.
    let config = VectorIndexConfig::new(dim, 16, 100, 8, 8, 200)
        .expect("config valid")
        .with_quantization("bf16");

    let build_start = Instant::now();
    let engine = vectors::UsearchEngine::create(&index_dir, config).expect("usearch create");
    let refs: Vec<(u32, &[f32])> = rows.iter().map(|(id, v)| (*id, v.as_slice())).collect();
    engine.insert_batch(&refs).expect("usearch insert");
    engine.build_index().expect("usearch build_index");
    let build_time_ms = build_start.elapsed().as_millis() as u64;

    bench_engine(&engine, queries, K, ground_truth, build_time_ms, &index_dir)
        .expect("usearch bench")
}

/// Prints a formatted comparison table to stdout.
fn print_comparison_table(
    dim: usize,
    lance: Option<&EngineBenchResult>,
    usearch: Option<&EngineBenchResult>,
) {
    println!("\n=== A/B Engine Benchmark (dim={dim}, k={K}, queries={QUERY_COUNT}) ===");
    println!(
        "{:<12} {:>10} {:>10} {:>10} {:>12} {:>14}",
        "Engine", "recall@k", "p50 (us)", "p95 (us)", "build (ms)", "index (KB)"
    );
    println!("{}", "-".repeat(70));
    if let Some(r) = lance {
        println!(
            "{:<12} {:>10.4} {:>10.1} {:>10.1} {:>12} {:>14.1}",
            "lance",
            r.recall_at_k,
            r.p50_us,
            r.p95_us,
            r.build_time_ms,
            r.index_size_bytes as f64 / 1024.0
        );
    }
    if let Some(r) = usearch {
        println!(
            "{:<12} {:>10.4} {:>10.1} {:>10.1} {:>12} {:>14.1}",
            "usearch",
            r.recall_at_k,
            r.p50_us,
            r.p95_us,
            r.build_time_ms,
            r.index_size_bytes as f64 / 1024.0
        );
    }
    println!("{}", "-".repeat(70));

    // Size delta.
    if let (Some(l), Some(u)) = (lance, usearch) {
        let delta = l.index_size_bytes as i64 - u.index_size_bytes as i64;
        println!(
            "Index size delta: usearch is {} bytes {} than lance",
            delta.abs(),
            if delta > 0 { "smaller" } else { "larger" }
        );
        let build_delta = l.build_time_ms as i64 - u.build_time_ms as i64;
        println!(
            "Build time delta: usearch is {} ms {} than lance",
            build_delta.abs(),
            if build_delta > 0 { "faster" } else { "slower" }
        );
    }
    println!();
}
