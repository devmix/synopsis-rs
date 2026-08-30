//! ADR 0003 machine gates at CI scale (task vectors 1.6; design D8, level 1).
//!
//! Two gates, same protocol family as the native-seam-spikes S3b run
//! (`docs/adr/spike-s3-results.md`), at a corpus size that keeps the debug
//! suite in seconds:
//!
//! 1. **recall@10 >= 0.95** — always run by `cargo test`. Correctness is
//!    build-profile independent, so it must hold in the debug profile too.
//!    Ground truth is exact L2 brute force computed in-process over the full
//!    corpus; the corpus and held-out queries are generated from a fixed
//!    embedded seed (SplitMix64 + Box-Muller, Gaussian cluster mixture on the
//!    unit sphere — the spike geometry).
//! 2. **p95 < 10 ms** — release-only. Debug timings are meaningless (spike
//!    lesson, design D8), so the test is `#[ignore]`-marked and refuses to
//!    run in a debug build. Run it with:
//!    `cargo test -p vectors --release -- --ignored`
//!
//! The recall test also exercises the task 1.4 lifecycle scenarios (batch
//! delete + idempotent re-delete, `chunk_ids`/`count` reconciliation,
//! close→reopen persistence) on this corpus, reusing the same index build.
//!
//! **Deviation from the ADR 0003 configuration (documented, task 1.6 note):**
//! `num_partitions = 64` instead of 256. At N = 20 000 a 256-partition IVF
//! would leave ~78 rows per partition and over-split the k-means centroids;
//! 64 keeps >= 300 rows per partition. `m = 16`, `ef_construction = 100`,
//! `nprobes = 32`, `ef_search = 200` stay at the ADR defaults, so the gate
//! measures the ADR query path (nprobes/efSearch) unchanged. The ADR gate
//! itself is re-validated at full scale (N = 1M, 256 partitions) in task 1.8.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::f64::consts::PI;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use vectors::{LanceEngine, VectorIndexConfig};

// ── Protocol constants ────────────────────────────────────────────────────────

/// Corpus size: N = 20K vectors (task 1.6: ~20K, kept >= 10K for meaningful
/// ANN behavior with the reduced partition count).
const N: usize = 20_000;
/// Vector dimension (bge-m3 registry dim, ADR 0003).
const DIM: usize = 1024;
/// Cluster count of the Gaussian-mixture geometry (spike: 256 for 250K;
/// 128 keeps ~156 points per cluster at N = 20K).
const N_CLUSTERS: usize = 128;
/// Per-dimension noise sigma added to a cluster center before re-normalization.
const NOISE_SIGMA: f64 = 0.1;
/// Held-out query count (spike protocol: generated after the corpus, never inserted).
const N_QUERIES: usize = 100;
/// ANN top-k for both gates (recall@10).
const TOP_K: usize = 10;
/// Warmup queries discarded before the latency test starts timing.
const WARMUP: usize = 10;
/// Embedded seed — reproducible corpus and held-out queries across runs and machines.
const SEED: u64 = 0x5EED_1600_0000_0016;
/// 2^-53 (Rust hex-float literals require a fractional part, so the literal form is used).
const INV_2_POW_53: f64 = 1.1102230246251565e-16;

// ── ADR 0003 gates ────────────────────────────────────────────────────────────

/// recall@10 gate.
const GATE_RECALL: f64 = 0.95;
/// p95 search latency gate in milliseconds.
const GATE_P95_MS: f64 = 10.0;

// ── Index configuration (see module docs for the documented deviation) ──────

/// ADR 0003 parameters with the task 1.6 partition-count reduction.
fn gate_config() -> VectorIndexConfig {
    VectorIndexConfig::new(DIM, 16, 100, 64, 32, 200).expect("gate config is valid")
}

// ── Deterministic PRNG (SplitMix64 + Box-Muller; no RNG crate in the palette) ─

/// Deterministic PRNG with a cached Box-Muller spare. The fixed SEED makes the
/// corpus byte-identical across runs, machines and toolchains (pure integer +
/// IEEE-754 ops; no platform-dependent randomness anywhere in generation).
struct Rng {
    state: u64,
    gauss_spare: Option<f64>,
}

impl Rng {
    fn new(seed: u64) -> Self {
        Self {
            state: seed,
            gauss_spare: None,
        }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform f64 in [0, 1).
    fn uniform_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * INV_2_POW_53
    }

    /// Standard normal sample (Box-Muller), deterministic.
    fn gauss(&mut self) -> f64 {
        if let Some(spare) = self.gauss_spare.take() {
            return spare;
        }
        let u1 = 1.0 - self.uniform_f64(); // in (0, 1], avoids log(0)
        let r = (-2.0 * u1.ln()).sqrt();
        let theta = 2.0 * PI * self.uniform_f64();
        self.gauss_spare = Some(r * theta.sin());
        r * theta.cos()
    }

    /// Uniform integer in [0, n) (modulo bias is irrelevant for cluster assignment).
    fn uniform_below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

// ── Corpus ────────────────────────────────────────────────────────────────────

/// Seeded synthetic fixture: flat corpus rows + held-out queries.
struct Corpus {
    /// N × DIM f32, row-major; each row L2-normalized at generation time.
    flat: Vec<f32>,
    /// N_QUERIES held-out unit vectors, never inserted into the index.
    queries: Vec<Vec<f32>>,
}

impl Corpus {
    /// The stored row for `id` (row-major slice into `flat`).
    fn row(&self, id: u32) -> &[f32] {
        let start = id as usize * DIM;
        &self.flat[start..start + DIM]
    }
}

/// One mixture draw: normalize(center + sigma * gauss), as an f32 row.
fn draw_point(rng: &mut Rng, centers: &[Vec<f64>]) -> Vec<f32> {
    let center = &centers[rng.uniform_below(N_CLUSTERS)];
    let mut n_sq = 0.0_f64;
    let mut row: Vec<f64> = Vec::with_capacity(DIM);
    for c in center.iter() {
        let x = c + NOISE_SIGMA * rng.gauss();
        row.push(x);
        n_sq += x * x;
    }
    let inv_n = 1.0 / n_sq.sqrt();
    row.into_iter().map(|x| (x * inv_n) as f32).collect()
}

/// Generate the fixture from SEED: N corpus rows then N_QUERIES held-out
/// queries, all drawn sequentially from one PRNG stream (queries are held out
/// by construction — they are never written to the index).
fn generate_corpus() -> Corpus {
    let mut rng = Rng::new(SEED);

    // Cluster centers: random unit vectors in f64.
    let mut centers: Vec<Vec<f64>> = Vec::with_capacity(N_CLUSTERS);
    for _ in 0..N_CLUSTERS {
        let v: Vec<f64> = (0..DIM).map(|_| rng.gauss()).collect();
        let norm = v.iter().map(|x| x * x).sum::<f64>().sqrt();
        centers.push(v.into_iter().map(|x| x / norm).collect());
    }

    let mut flat: Vec<f32> = Vec::with_capacity(N * DIM);
    for _ in 0..N {
        flat.extend(draw_point(&mut rng, &centers));
    }
    let queries = (0..N_QUERIES)
        .map(|_| draw_point(&mut rng, &centers))
        .collect();
    Corpus { flat, queries }
}

// ── Ground truth (exact L2 brute force, in-process) ──────────────────────────

/// Exact top-k ids by squared L2 distance for one query over the full corpus.
/// f64 accumulation (more accurate ground truth than the spike's f32).
fn exact_top_k_ids(corpus: &Corpus, query: &[f32], k: usize) -> Vec<u32> {
    let mut dists: Vec<(f64, u32)> = Vec::with_capacity(N);
    for i in 0..N {
        let row = corpus.row(i as u32);
        let mut acc: f64 = 0.0;
        for (a, b) in query.iter().zip(row.iter()) {
            let d = *a as f64 - *b as f64;
            acc += d * d;
        }
        dists.push((acc, i as u32));
    }
    // total_cmp keeps the order deterministic even on exact ties.
    dists.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
    dists.into_iter().take(k).map(|(_, id)| id).collect()
}

/// Exact ground truth for every held-out query, parallel over workers.
fn exact_top_k_all(corpus: &Corpus, k: usize) -> Vec<Vec<u32>> {
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(corpus.queries.len());
    std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(workers);
        for w in 0..workers {
            let start = (w * corpus.queries.len()) / workers;
            let end = ((w + 1) * corpus.queries.len()) / workers;
            if start >= end {
                continue;
            }
            handles.push(s.spawn(move || {
                corpus.queries[start..end]
                    .iter()
                    .map(|q| exact_top_k_ids(corpus, q, k))
                    .collect::<Vec<_>>()
            }));
        }
        let mut out = Vec::with_capacity(corpus.queries.len());
        for handle in handles {
            // A worker panic means broken corpus access; fail the test hard.
            out.extend(handle.join().expect("brute-force worker panicked"));
        }
        out
    })
}

// ── Latency percentiles (nearest-rank, spike protocol) ───────────────────────

/// Nearest-rank percentile of a non-empty, ascending-sorted sample.
fn percentile(sorted: &[f64], pct: usize) -> f64 {
    let rank = (sorted.len() as f64 * pct as f64 / 100.0).ceil() as usize;
    sorted[rank - 1]
}

// ── Test infrastructure ───────────────────────────────────────────────────────

/// A unique temporary directory that removes itself on drop (the same pattern
/// as the engine unit tests; `tempfile` is not in the frozen palette).
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "synopsis-vectors-gates-{}-{}",
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

/// Insert the whole corpus into `engine` (chunk ids are 0..N).
fn load_corpus(engine: &LanceEngine, corpus: &Corpus) {
    let rows: Vec<(u32, &[f32])> = (0..N as u32).map(|id| (id, corpus.row(id))).collect();
    engine.insert_batch(&rows).expect("insert corpus");
}

// ── Gate 1: recall@10 + task 1.4 lifecycle scenarios (always run) ────────────

#[test]
fn recall_gate_with_delete_and_persistence() {
    let t0 = Instant::now();
    let corpus = generate_corpus();
    eprintln!(
        "gates: corpus {}×{} generated in {} ms (seed {SEED:#x}, K={N_CLUSTERS}, sigma={NOISE_SIGMA})",
        N,
        DIM,
        t0.elapsed().as_millis(),
    );

    let dir = TempDir::new();
    let config = gate_config();
    // `config` is reused for the reopen below, so clone it into `create`.
    let engine = LanceEngine::create(&dir.0, config.clone()).expect("create engine");
    load_corpus(&engine, &corpus);
    engine.build_index().expect("build IvfHnswSq index");
    assert_eq!(engine.count().expect("count"), N as u64);

    // Index sanity: a stored row's own vector must be its top-1 at ~0 distance.
    for id in [0u32, 1000, 9999, 19_999] {
        let results = engine.search(corpus.row(id), 1).expect("search own row");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, id, "top-1 must be the queried row");
        assert!(
            results[0].1 < 1e-4,
            "top-1 distance ~0, got {}",
            results[0].1
        );
    }

    // Ground truth (parallel, untimed).
    let t0 = Instant::now();
    let exact = exact_top_k_all(&corpus, TOP_K);
    eprintln!(
        "gates: exact-L2 ground truth ({} queries × {} rows × {} dims) in {} ms",
        N_QUERIES,
        N,
        DIM,
        t0.elapsed().as_millis(),
    );

    // recall@10: |ANN top-10 ∩ exact top-10| per query, averaged.
    let mut hits: usize = 0;
    for (qi, query) in corpus.queries.iter().enumerate() {
        let results = engine.search(query, TOP_K).expect("search");
        assert_eq!(results.len(), TOP_K, "10 results for 20K stored rows");
        for (id, _) in &results {
            if exact[qi].contains(id) {
                hits += 1;
            }
        }
    }
    let recall = hits as f64 / (N_QUERIES * TOP_K) as f64;
    println!(
        "GATE recall@{TOP_K} = {recall:.4} (gate >= {GATE_RECALL}) — first query head [{}]",
        corpus.queries[0]
            .iter()
            .take(4)
            .map(|x| format!("{x:.6}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    assert!(
        recall >= GATE_RECALL,
        "recall@{TOP_K} {recall:.4} is below the gate {GATE_RECALL}"
    );

    // ── Task 1.4 lifecycle scenarios on this corpus ─────────────────────────
    // Batch delete: every 97th id (~206 rows), idempotent re-delete on top.
    let to_delete: Vec<u32> = (0..N as u32).filter(|id| id % 97 == 0).collect();
    engine.delete_by_chunk_ids(&to_delete).expect("delete");
    engine
        .delete_by_chunk_ids(&to_delete)
        .expect("repeat delete of the same ids");

    // Reconciliation primitives must be exact: count and full id listing.
    assert_eq!(engine.count().expect("count"), (N - to_delete.len()) as u64);
    let mut ids = engine.chunk_ids().expect("chunk_ids");
    ids.sort_unstable();
    let expected: Vec<u32> = (0..N as u32).filter(|id| id % 97 != 0).collect();
    assert_eq!(ids, expected, "chunk_ids must be exactly the undeleted ids");

    // Deleted rows must not surface in search: sample 32 deleted ids spread
    // across the corpus and search with each row's own vector.
    for &id in to_delete.iter().step_by(4).take(32) {
        let results = engine
            .search(corpus.row(id), TOP_K)
            .expect("search deleted row");
        assert!(
            !results.iter().any(|(rid, _)| *rid == id),
            "deleted id {id} must not be returned"
        );
    }

    // Persistence: close→reopen must see the data and the deletions.
    let top1_before = engine
        .search(&corpus.queries[0], 1)
        .expect("search before reopen")
        .pop()
        .expect("one result")
        .0;
    drop(engine);

    let engine = LanceEngine::open(&dir.0, config).expect("reopen");
    assert_eq!(engine.count().expect("count"), (N - to_delete.len()) as u64);
    let ids = engine.chunk_ids().expect("chunk_ids after reopen");
    assert!(
        ids.iter().all(|id| id % 97 != 0),
        "deletions must persist across reopen"
    );
    let top1_after = engine
        .search(&corpus.queries[0], 1)
        .expect("search after reopen")
        .pop()
        .expect("one result")
        .0;
    assert_eq!(top1_before, top1_after, "reopen must see the same index");
}

// ── Gate 2: p95 latency (release-only) ───────────────────────────────────────

/// Run with `cargo test -p vectors --release -- --ignored`.
#[ignore = "release-only: debug timings are meaningless (design D8)"]
#[test]
fn p95_latency_gate_release_only() {
    if cfg!(debug_assertions) {
        panic!("run under --release: cargo test -p vectors --release -- --ignored");
    }

    let corpus = generate_corpus();
    let dir = TempDir::new();
    let engine = LanceEngine::create(&dir.0, gate_config()).expect("create engine");
    load_corpus(&engine, &corpus);
    engine.build_index().expect("build IvfHnswSq index");

    // Warmup: first WARMUP queries, discarded (cache and index-path warmup).
    for query in &corpus.queries[..WARMUP] {
        engine.search(query, TOP_K).expect("warmup search");
    }

    // Timed: every held-out query through the full sync facade (block_on +
    // async search path — the runtime overhead is part of the measured path,
    // per ADR 0003 note 4).
    let mut latencies_ms: Vec<f64> = Vec::with_capacity(N_QUERIES);
    for query in &corpus.queries {
        let t0 = Instant::now();
        let results = engine.search(query, TOP_K).expect("search");
        latencies_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(results.len(), TOP_K);
    }
    latencies_ms.sort_by(|a, b| a.total_cmp(b));
    let p50 = percentile(&latencies_ms, 50);
    let p95 = percentile(&latencies_ms, 95);
    println!(
        "GATE p50 = {p50:.3} ms, p95 = {p95:.3} ms over {N_QUERIES} queries (gate p95 < {GATE_P95_MS} ms)"
    );
    assert!(
        p95 < GATE_P95_MS,
        "p95 {p95:.3} ms is above the gate {GATE_P95_MS} ms (p50 {p50:.3} ms)"
    );
}
