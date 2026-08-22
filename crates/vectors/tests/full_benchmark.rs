//! Full-scale benchmark (change `vectors`, task 1.8) — closes ADR 0003 open
//! question №2: full N=1M scale and real-geometry repeat.
//!
//! Repeats the s3b_lance spike protocol at full scale against the production
//! [`LanceEngine`] public API (the same code path the product uses):
//!
//! - seeded synthetic corpus N=1M×1024 f32 (256-cluster Gaussian mixture on
//!   the unit sphere, σ=0.1 — the spike's geometry family; N overridable via
//!   `SYNOPSIS_BENCH_N`),
//! - index: `VectorIndexConfig::default()` (ADR 0003: IvfHnswSq u8-SQ, M=16,
//!   efConstruction=100, num_partitions=256, nprobes=32, efSearch=200, L2),
//! - 100 held-out queries (generated, never inserted), top-k=50, warmup 10,
//! - recall@10 vs exact-L2 brute force over the full corpus (in-process),
//! - two independent runs A/B in fresh work dirs — p50/p95 spread < 5 % gate,
//! - peak RSS delta during the search phase (Linux `/proc/self/status`),
//! - on-disk size (table + index).
//!
//! Memory plan: unlike the spike (250K corpus held in RAM), the 1M corpus
//! (~4 GB) is NEVER materialized — every row is a pure function of its index
//! (per-row SplitMix64 seed, same PRNG as the spike), regenerated on the fly
//! for insert and brute-force ground truth. Transient memory is one insert
//! batch (~4 MB) plus one ground-truth distance list (~16 MB).
//!
//! Gates (ADR 0003 / openspec config): p95 < 10 ms, recall@10 ≥ 0.95,
//! RSS delta ≤ ~2 GB. This harness PRINTS gate verdicts but does NOT assert
//! on them: the full-scale result is a decision input for the ADR appendix —
//! a gate miss is escalated to the human (ADR mitigation: raise
//! efSearch/nprobes, runtime parameters), not a test failure. Invariants
//! (row count, corpus fingerprint, result length) ARE asserted.
//!
//! # Usage (release build is MANDATORY — debug timings are meaningless)
//!
//! ```sh
//! cargo test -p vectors --release -- --ignored full_benchmark_synthetic --nocapture
//! ```
//!
//! Env: `SYNOPSIS_BENCH_N` (default 1_000_000), `SYNOPSIS_BENCH_WORK_DIR`
//! (default `/mnt/local/sandbox/opencode-vectors-18`; `/tmp` is a tmpfs on
//! this machine and must NOT hold the ~5.3 GB dataset). Run A's directory is
//! removed before run B starts, so disk holds one run at a time.
//!
//! # Run (b): real oracle fixture
//!
//! ```sh
//! SYNOPSIS_BENCH_VECTORS_BIN=<oracle vectors.bin> \
//!   cargo test -p vectors --release -- --ignored full_benchmark_real_fixture --nocapture
//! ```
//!
//! Deferred while the oracle export is unavailable (task 1.8: not an archive
//! blocker; the test prints a DEFERRED line and passes). Queries there are
//! seeded-random corpus rows (the export contains only the corpus, no
//! held-out set) — a documented deviation from the held-out protocol.

// Benchmark harness: deterministic fixtures make unwrap/expect safe.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::f64::consts::PI;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use vectors::synx;
use vectors::{LanceEngine, VectorIndexConfig};

// ── Protocol constants (s3b_lance spike protocol; ADR 0003 config) ────────────

/// Vector dimension (bge-m3 registry dim).
const DIM: usize = 1024;
/// Held-out query count.
const N_QUERIES: usize = 100;
/// ANN top-k per query.
const TOP_K: usize = 50;
/// Recall depth for the acceptance metric (recall@10).
const RECALL_AT: usize = 10;
/// Warmup queries discarded before timing (the timed phase then covers all
/// N_QUERIES, exactly as in the spike).
const WARMUP: usize = 10;
/// Embedded seed (the spike's seed) — reproducible corpus across runs/machines.
const SEED: u64 = 0x5EED_3B25_CAFE_BAB7;
/// Seed base for held-out queries (disjoint from the corpus seed space).
const QUERY_SEED: u64 = 0x9E37_79B9_7F4A_7C15;
/// Seed for the cluster-center generation stream.
const CENTER_SEED: u64 = 0xC0FFEE_00C0FFEE;
/// Cluster count of the Gaussian-mixture geometry (spike: 256).
const N_CLUSTERS: usize = 256;
/// Per-dimension noise sigma added to a cluster center before re-normalization.
const NOISE_SIGMA: f64 = 0.1;
/// Rows per `insert_batch` call (the engine's own Arrow batch size, design D4).
const BATCH_ROWS: usize = 1000;
/// 2^-53 as a decimal literal (Rust hex-float literals require a fractional part).
const INV_2_POW_53: f64 = 1.1102230246251565e-16;
/// FNV-1a 64-bit prime (2^40 + 2^8 + 0xB3) for the corpus fingerprint.
const FNV64_PRIME: u64 = 0x0000_0100_0000_01B3;

// ── Gates (ADR 0003 / openspec config) ────────────────────────────────────────

/// p95 search latency gate in milliseconds.
const GATE_P95_MS: f64 = 10.0;
/// recall@10 gate.
const GATE_RECALL: f64 = 0.95;
/// Peak-RSS-delta gate during the search phase, MB (~2 GB).
const GATE_RSS_MB: u64 = 2048;
/// A/B reproducibility gate: max relative spread of p50/p95, percent.
const GATE_SPREAD_PCT: f64 = 5.0;

// ── Deterministic PRNG (SplitMix64 + Box–Muller, ported from the spike) ──────

/// Deterministic SplitMix64 PRNG with a cached Box–Muller spare. Pure integer
/// + IEEE-754 ops: byte-identical streams across runs, machines, toolchains.
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
        let bits = self.next_u64() >> 11; // top 53 bits
        bits as f64 * INV_2_POW_53
    }

    /// Standard normal sample (Box–Muller), deterministic.
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

    /// Uniform integer in [0, n) (modulo bias is irrelevant for cluster picks).
    fn uniform_below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// N cluster centers: random unit vectors in f64 (the spike's generation).
fn make_centers() -> Vec<Vec<f64>> {
    let mut rng = Rng::new(CENTER_SEED);
    (0..N_CLUSTERS)
        .map(|_| {
            let v: Vec<f64> = (0..DIM).map(|_| rng.gauss()).collect();
            let n = v.iter().map(|x| x * x).sum::<f64>().sqrt();
            v.into_iter().map(|x| x / n).collect()
        })
        .collect()
}

/// Corpus row `i`: normalize(center + σ·gauss), an f32 row. Row `i` depends
/// only on `i` and the fixed centers — regeneratable without materializing
/// the corpus (per-row seed = SEED + i, the SplitMix64 counter pattern).
fn make_row(centers: &[Vec<f64>], i: u64) -> Vec<f32> {
    let mut rng = Rng::new(SEED.wrapping_add(i));
    draw(&mut rng, centers)
}

/// Held-out query `q`: same draw, disjoint seed base (never inserted).
fn make_query(centers: &[Vec<f64>], q: u64) -> Vec<f32> {
    let mut rng = Rng::new(QUERY_SEED.wrapping_add(q));
    draw(&mut rng, centers)
}

/// One mixture draw: normalize(center + sigma * gauss).
fn draw(rng: &mut Rng, centers: &[Vec<f64>]) -> Vec<f32> {
    let center = &centers[rng.uniform_below(N_CLUSTERS)];
    let mut n_sq = 0.0_f64;
    let mut row: Vec<f64> = Vec::with_capacity(DIM);
    for c in center {
        let x = c + NOISE_SIGMA * rng.gauss();
        row.push(x);
        n_sq += x * x;
    }
    let inv_n = 1.0 / n_sq.sqrt();
    row.into_iter().map(|x| (x * inv_n) as f32).collect()
}

// ── Fingerprint ───────────────────────────────────────────────────────────────

/// Streaming FNV-1a 64 — corpus determinism evidence (spike: FINGERPRINT line).
#[derive(Default)]
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(FNV64_PRIME);
        }
    }
    fn finish(self) -> u64 {
        self.0
    }
}

/// FNV-1a over an f32 slice (native-endian bytes; same machine across runs).
fn fingerprint_f32(values: &[f32]) -> u64 {
    let mut h = Fnv::new();
    for x in values {
        h.feed(&x.to_ne_bytes());
    }
    h.finish()
}

// ── Ground truth (exact L2 brute force, rows regenerated on the fly) ─────────

/// Exact top-RECALL_AT by L2² for `queries` over the full N-row corpus.
/// Parallel over worker threads (each returns its slice; joined in order);
/// each worker regenerates corpus rows on the fly (no 4 GB corpus in RAM).
/// L2² accumulation is f32 (the spike's metric family); stored as f64 for a
/// deterministic total_cmp sort.
fn brute_force_all(
    centers: &[Vec<f64>],
    n: usize,
    queries: &[Vec<f32>],
    workers: usize,
) -> Vec<Vec<u32>> {
    std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(workers);
        for w in 0..workers {
            let start = (w * queries.len()) / workers;
            let end = ((w + 1) * queries.len()) / workers;
            if start >= end {
                continue;
            }
            handles.push(s.spawn(move || {
                let mut part: Vec<Vec<u32>> = Vec::with_capacity(end - start);
                for q in &queries[start..end] {
                    let mut all: Vec<(f64, u32)> = Vec::with_capacity(n);
                    for i in 0..n as u64 {
                        let row = make_row(centers, i);
                        let mut acc: f32 = 0.0;
                        for (a, b) in q.iter().zip(row.iter()) {
                            let d = a - b;
                            acc += d * d;
                        }
                        all.push((acc as f64, i as u32));
                    }
                    all.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
                    part.push(all.into_iter().take(RECALL_AT).map(|(_, i)| i).collect());
                }
                part
            }));
        }
        let mut out: Vec<Vec<u32>> = Vec::with_capacity(queries.len());
        for h in handles {
            out.extend(h.join().expect("ground-truth worker"));
        }
        out
    })
}

// ── Machine measurements ──────────────────────────────────────────────────────

/// VmRSS of this process in MB (Linux /proc/self/status). The laptop target is
/// Linux; a missing file is a hard failure (the spike's stance).
fn vm_rss_mb() -> u64 {
    let status = fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .trim()
                .trim_end_matches(" kB")
                .parse::<u64>()
                .expect("parse VmRSS")
                / 1024;
        }
    }
    panic!("VmRSS not found in /proc/self/status");
}

/// (total bytes under dir, bytes under `<dir>/vectors.lance/_indices`).
fn dir_sizes(dir: &Path) -> (u64, u64) {
    let mut total = 0_u64;
    let mut index_part = 0_u64;
    let indices_prefix = "vectors.lance/_indices/";
    walk(dir, &mut |p| {
        if p.is_file() {
            let len = fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            total += len;
            if p.to_string_lossy().contains(indices_prefix) {
                index_part += len;
            }
        }
    });
    (total, index_part)
}

/// Recursive file walk (std-only).
fn walk(dir: &Path, f: &mut impl FnMut(&Path)) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            walk(&p, f);
        } else {
            f(&p);
        }
    }
}

/// Nearest-rank p50/p95 (microseconds in, milliseconds out), the spike's rule.
fn percentile_pair(lat_us: &mut [f64]) -> (f64, f64) {
    lat_us.sort_by(|a, b| a.total_cmp(b));
    let n = lat_us.len();
    let at =
        |p: usize| lat_us[usize::min(n, ((n as f64 * p as f64 / 100.0).ceil()) as usize) - 1] / 1e3;
    (at(50), at(95))
}

// ── Search phase ──────────────────────────────────────────────────────────────

/// One run's measured metrics (the A/B comparison inputs; the write/build
/// and on-disk numbers are emitted in the METRIC line for the ADR appendix).
struct RunMetrics {
    p50_ms: f64,
    p95_ms: f64,
    recall_at_10: f64,
    rss_delta_mb: u64,
    fingerprint: u64,
}

/// Search phase against a built index: RSS baseline → WARMUP untimed queries
/// → all N_QUERIES timed (spike protocol) → p50/p95, recall@RECALL_AT vs the
/// exact ground truth, peak RSS delta.
fn measure_search(
    engine: &LanceEngine,
    queries: &[Vec<f32>],
    exact: &[Vec<u32>],
) -> (f64, f64, f64, u64) {
    let rss_base = vm_rss_mb();
    for q in &queries[..WARMUP] {
        engine.search(q, TOP_K).expect("warmup search"); // discarded
    }
    let mut lat_us: Vec<f64> = Vec::with_capacity(queries.len());
    let mut hits: u32 = 0;
    let mut rss_max = rss_base;
    for (qi, q) in queries.iter().enumerate() {
        let t0 = Instant::now();
        let results = engine.search(q, TOP_K).expect("search");
        lat_us.push(t0.elapsed().as_secs_f64() * 1e6);
        rss_max = rss_max.max(vm_rss_mb());
        assert_eq!(results.len(), TOP_K, "top-{TOP_K} for a 1M-row index");
        // recall@RECALL_AT: ANN top-10 vs the exact top-10 set.
        for (id, _) in results.iter().take(RECALL_AT) {
            if exact[qi].contains(id) {
                hits += 1;
            }
        }
    }
    let (p50_ms, p95_ms) = percentile_pair(&mut lat_us);
    let recall = hits as f64 / (queries.len() * RECALL_AT) as f64;
    (p50_ms, p95_ms, recall, rss_max.saturating_sub(rss_base))
}

/// One full run in a fresh work dir: insert (fingerprinting) → build →
/// search phase. Returns the measured metrics.
fn run_once(
    label: &str,
    work_dir: &Path,
    n: usize,
    centers: &[Vec<f64>],
    queries: &[Vec<f32>],
    exact: &[Vec<u32>],
) -> RunMetrics {
    let dir = work_dir.join(label);
    if dir.exists() {
        fs::remove_dir_all(&dir).expect("clear previous run dir");
    }

    // 1) Insert the corpus (rows regenerated on the fly, batched).
    let t0 = Instant::now();
    let engine = LanceEngine::create(&dir, VectorIndexConfig::default()).expect("create engine");
    let mut batch: Vec<(u32, Vec<f32>)> = Vec::with_capacity(BATCH_ROWS);
    let mut fingerprint = Fnv::new();
    for i in 0..n as u64 {
        let row = make_row(centers, i);
        for x in &row {
            fingerprint.feed(&x.to_ne_bytes());
        }
        batch.push((i as u32, row));
        if batch.len() == BATCH_ROWS {
            let refs: Vec<(u32, &[f32])> =
                batch.iter().map(|(id, v)| (*id, v.as_slice())).collect();
            engine.insert_batch(&refs).expect("insert batch");
            batch.clear();
        }
    }
    if !batch.is_empty() {
        let refs: Vec<(u32, &[f32])> = batch.iter().map(|(id, v)| (*id, v.as_slice())).collect();
        engine.insert_batch(&refs).expect("insert tail batch");
    }
    let write_ms = t0.elapsed().as_millis();
    assert_eq!(
        engine.count().expect("count"),
        n as u64,
        "row count after insert"
    );

    // 2) Build the IvfHnswSq index (ADR 0003 config).
    let t0 = Instant::now();
    engine.build_index().expect("build IvfHnswSq index");
    let build_ms = t0.elapsed().as_millis();

    // 3) On-disk sizes.
    let (total_bytes, index_bytes) = dir_sizes(&dir);

    // 4) Search phase.
    let (p50_ms, p95_ms, recall_at_10, rss_delta_mb) = measure_search(&engine, queries, exact);
    let fp = fingerprint.finish();

    println!(
        "METRIC run={label} n={n} dim={DIM} write_ms={write_ms} build_ms={build_ms} search_p50_ms={p50_ms:.3} search_p95_ms={p95_ms:.3} recall_at_10={recall_at_10:.4} rss_delta_mb={rss_delta_mb} index_bytes={index_bytes} total_bytes={total_bytes} fingerprint={fp:016x}"
    );
    let pass = p95_ms < GATE_P95_MS && recall_at_10 >= GATE_RECALL && rss_delta_mb <= GATE_RSS_MB;
    println!(
        "GATE run={label} p95={p95_ms:.3} ms (< {GATE_P95_MS}) recall@10={recall_at_10:.4} (>= {GATE_RECALL}) rss_delta={rss_delta_mb} MB (<= {GATE_RSS_MB}) → {}",
        if pass { "PASS" } else { "FAIL" }
    );

    RunMetrics {
        p50_ms,
        p95_ms,
        recall_at_10,
        rss_delta_mb,
        fingerprint: fp,
    }
}

// ── Mitigation sweep (ADR 0003 mitigation #1) ────────────────────────────────

/// (nprobes, efSearch) points of the mitigation sweep: the ADR's runtime
/// levers (raise nprobes/efSearch — no index rebuild required).
const SWEEP_POINTS: [(usize, usize); 5] =
    [(64, 200), (128, 200), (256, 200), (128, 400), (256, 400)];

/// Re-measures the search phase on an already-built run index with raised
/// nprobes/efSearch (spike `--reuse` diagnostic, ported). Opens the same
/// table directory with a search-only config; the on-disk index is untouched.
fn mitigation_sweep(index_dir: &Path, queries: &[Vec<f32>], exact: &[Vec<u32>]) {
    for &(nprobes, ef) in &SWEEP_POINTS {
        let config = VectorIndexConfig::new(DIM, 16, 100, 256, nprobes, ef).expect("sweep config");
        let engine = LanceEngine::open(index_dir, config).expect("open run index");
        let (p50_ms, p95_ms, recall_at_10, rss_delta_mb) = measure_search(&engine, queries, exact);
        let pass =
            p95_ms < GATE_P95_MS && recall_at_10 >= GATE_RECALL && rss_delta_mb <= GATE_RSS_MB;
        println!(
            "METRIC run=B_nprobes{nprobes}_ef{ef} search_p50_ms={p50_ms:.3} search_p95_ms={p95_ms:.3} recall_at_10={recall_at_10:.4} rss_delta_mb={rss_delta_mb}"
        );
        println!(
            "GATE nprobes={nprobes} ef={ef} p95={p95_ms:.3} ms (< {GATE_P95_MS}) recall@10={recall_at_10:.4} (>= {GATE_RECALL}) rss_delta={rss_delta_mb} MB (<= {GATE_RSS_MB}) → {}",
            if pass { "PASS" } else { "FAIL" }
        );
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Protocol guard: debug timings are meaningless (the spike's lesson). The
/// constant-conditional assert is intentional — it fires only in dev builds.
#[allow(clippy::assertions_on_constants)]
fn assert_release() {
    assert!(
        !cfg!(debug_assertions),
        "protocol: release build is mandatory — run with --release"
    );
}

fn bench_n() -> usize {
    std::env::var("SYNOPSIS_BENCH_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000_000)
}

fn work_dir() -> PathBuf {
    std::env::var("SYNOPSIS_BENCH_WORK_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/mnt/local/sandbox/opencode-vectors-18"))
}

/// Run (a): synthetic N=1M×1024, ADR 0003 config, two runs A/B.
#[test]
#[ignore = "full-scale benchmark: release-only, ~20-40 min, ~6 GB disk"]
fn full_benchmark_synthetic() {
    assert_release();
    let n = bench_n();
    let dir = work_dir();
    fs::create_dir_all(&dir).expect("create work dir");
    let config = VectorIndexConfig::default();
    println!(
        "INFO n={n} dim={DIM} work_dir={} config={config:?} (ADR 0003 defaults)",
        dir.display()
    );

    // 1) Held-out queries + cluster centers (deterministic).
    let t0 = Instant::now();
    let centers = make_centers();
    let queries: Vec<Vec<f32>> = (0..N_QUERIES as u64)
        .map(|q| make_query(&centers, q))
        .collect();
    println!(
        "INFO generated {N_QUERIES} held-out queries in {} ms (seed {SEED:#x}, mixture K={N_CLUSTERS} sigma={NOISE_SIGMA})",
        t0.elapsed().as_millis()
    );

    // 2) Exact ground truth (once — the corpus is identical for runs A and B).
    let workers = std::thread::available_parallelism()
        .map(|x| x.get())
        .unwrap_or(4)
        .min(N_QUERIES);
    let t0 = Instant::now();
    let exact = brute_force_all(&centers, n, &queries, workers);
    println!(
        "INFO brute-force ground truth (exact L2² top-{RECALL_AT} × {N_QUERIES} queries over {n}×{DIM}, {workers} workers) took {} ms",
        t0.elapsed().as_millis()
    );

    // 3) Two independent runs (A's dir is removed before B: disk holds one
    //    ~5.3 GB run at a time on this machine).
    let a = run_once("A", &dir, n, &centers, &queries, &exact);
    fs::remove_dir_all(dir.join("A")).expect("remove run A before run B");
    let b = run_once("B", &dir, n, &centers, &queries, &exact);

    // Optional mitigation sweep on run B's index (SYNOPSIS_BENCH_SWEEP=1):
    // quantifies the ADR's runtime levers (nprobes/efSearch, no rebuild).
    if std::env::var("SYNOPSIS_BENCH_SWEEP").ok().as_deref() == Some("1") {
        println!("INFO mitigation sweep on run B index (nprobes/efSearch, no rebuild)");
        mitigation_sweep(&dir.join("B"), &queries, &exact);
    }

    // 4) Corpus determinism: both runs inserted byte-identical corpora.
    assert_eq!(
        a.fingerprint, b.fingerprint,
        "runs A and B must insert identical corpora"
    );
    println!("FINGERPRINT corpus_fnv1a={:016x} (A == B)", a.fingerprint);

    // 5) A/B reproducibility gate (< 5 % spread, the spike's rule: (max-min)/min).
    let spread = |x: f64, y: f64| (x.max(y) - x.min(y)) / x.min(y) * 100.0;
    let p50_spread = spread(a.p50_ms, b.p50_ms);
    let p95_spread = spread(a.p95_ms, b.p95_ms);
    let repro_ok = p50_spread < GATE_SPREAD_PCT && p95_spread < GATE_SPREAD_PCT;
    println!(
        "REPRO p50 {:.3}/{:.3} ms ({p50_spread:.1} %) p95 {:.3}/{:.3} ms ({p95_spread:.1} %) → {}",
        a.p50_ms,
        b.p50_ms,
        a.p95_ms,
        b.p95_ms,
        if repro_ok { "PASS" } else { "FAIL" }
    );

    // 6) Verdict (informational — the decision is recorded in the ADR appendix).
    let both_pass = a.p95_ms < GATE_P95_MS
        && b.p95_ms < GATE_P95_MS
        && a.recall_at_10 >= GATE_RECALL
        && b.recall_at_10 >= GATE_RECALL
        && a.rss_delta_mb <= GATE_RSS_MB
        && b.rss_delta_mb <= GATE_RSS_MB;
    println!(
        "VERDICT {}: full-scale result recorded in docs/adr/0003-ann-engine.md (appendix)",
        if both_pass && repro_ok {
            "GO — all gates pass in both runs"
        } else {
            "ESCALATE — gate miss or reproducibility spread; see ADR appendix"
        }
    );
}

/// Run (b): real oracle fixture (`vectors.bin` via the synx loader). Deferred
/// while the oracle export is unavailable — not an archive blocker (task 1.8).
#[test]
#[ignore = "full-scale benchmark on the real oracle fixture: release-only"]
fn full_benchmark_real_fixture() {
    assert_release();
    let path = match std::env::var("SYNOPSIS_BENCH_VECTORS_BIN").ok() {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => {
            println!(
                "DEFERRED run=real-fixture reason=\"oracle vectors.bin export unavailable (SYNOPSIS_BENCH_VECTORS_BIN not set) — task 1.8: not an archive blocker\""
            );
            return;
        }
    };

    // Stream the fixture (bounded memory per row) and buffer it: the exact
    // ground truth needs repeated full-corpus passes. ~4 GB at 1M×1024.
    let t0 = Instant::now();
    let file = fs::File::open(&path).expect("open oracle fixture");
    let reader = synx::open(file).expect("SYNX header");
    assert_eq!(reader.dim() as usize, DIM, "fixture must be bge-m3 dim");
    let n = reader.row_count() as usize;
    let mut ids: Vec<u32> = Vec::with_capacity(n);
    let mut flat: Vec<f32> = Vec::with_capacity(n * DIM);
    for row in reader {
        let (id, vector) = row.expect("fixture row");
        ids.push(id);
        flat.extend(vector);
    }
    let fingerprint = fingerprint_f32(&flat);
    println!(
        "INFO loaded {}×{DIM} fixture from {} in {} ms (fingerprint {:016x})",
        n,
        path.display(),
        t0.elapsed().as_millis(),
        fingerprint
    );

    // Queries: seeded-random corpus rows (the export has no held-out set —
    // documented deviation; recall@10 vs exact top-10 is still well-defined).
    let mut qrng = Rng::new(QUERY_SEED);
    let queries: Vec<Vec<f32>> = (0..N_QUERIES)
        .map(|_| {
            let i = qrng.uniform_below(n);
            flat[i * DIM..(i + 1) * DIM].to_vec()
        })
        .collect();

    // Ground truth over the buffered corpus (spike-style, corpus in RAM).
    let workers = std::thread::available_parallelism()
        .map(|x| x.get())
        .unwrap_or(4)
        .min(N_QUERIES);
    let exact: Vec<Vec<u32>> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(workers);
        for w in 0..workers {
            let start = (w * N_QUERIES) / workers;
            let end = ((w + 1) * N_QUERIES) / workers;
            if start >= end {
                continue;
            }
            // Copyable references into the move closure (the owned `flat` and
            // `queries` stay in the test; thread::scope keeps them alive).
            let qslice: &[Vec<f32>] = &queries[start..end];
            let corpus: &[f32] = &flat;
            handles.push(s.spawn(move || {
                let mut part: Vec<Vec<u32>> = Vec::with_capacity(qslice.len());
                for q in qslice {
                    let mut all: Vec<(f64, u32)> = Vec::with_capacity(n);
                    for (i, row) in corpus.chunks_exact(DIM).enumerate() {
                        let mut acc: f32 = 0.0;
                        for (a, b) in q.iter().zip(row.iter()) {
                            let d = a - b;
                            acc += d * d;
                        }
                        all.push((acc as f64, i as u32));
                    }
                    all.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
                    part.push(all.into_iter().take(RECALL_AT).map(|(_, i)| i).collect());
                }
                part
            }));
        }
        let mut out: Vec<Vec<u32>> = Vec::with_capacity(N_QUERIES);
        for h in handles {
            out.extend(h.join().expect("ground-truth worker"));
        }
        out
    });

    // Insert (ids come from the fixture; the engine keys by chunk_id).
    let dir = work_dir().join("real-fixture");
    if dir.exists() {
        fs::remove_dir_all(&dir).expect("clear previous fixture run dir");
    }
    let t0 = Instant::now();
    let engine = LanceEngine::create(&dir, VectorIndexConfig::default()).expect("create engine");
    let mut batch: Vec<(u32, Vec<f32>)> = Vec::with_capacity(BATCH_ROWS);
    for i in 0..n {
        let row = flat[i * DIM..(i + 1) * DIM].to_vec();
        batch.push((ids[i], row));
        if batch.len() == BATCH_ROWS {
            let refs: Vec<(u32, &[f32])> =
                batch.iter().map(|(id, v)| (*id, v.as_slice())).collect();
            engine.insert_batch(&refs).expect("insert batch");
            batch.clear();
        }
    }
    if !batch.is_empty() {
        let refs: Vec<(u32, &[f32])> = batch.iter().map(|(id, v)| (*id, v.as_slice())).collect();
        engine.insert_batch(&refs).expect("insert tail batch");
    }
    let write_ms = t0.elapsed().as_millis();
    assert_eq!(engine.count().expect("count"), n as u64, "row count");

    let t0 = Instant::now();
    engine.build_index().expect("build IvfHnswSq index");
    let build_ms = t0.elapsed().as_millis();
    let (total_bytes, index_bytes) = dir_sizes(&dir);

    let (p50_ms, p95_ms, recall_at_10, rss_delta_mb) = measure_search(&engine, &queries, &exact);
    println!(
        "METRIC run=real-fixture n={n} dim={DIM} write_ms={write_ms} build_ms={build_ms} search_p50_ms={p50_ms:.3} search_p95_ms={p95_ms:.3} recall_at_10={recall_at_10:.4} rss_delta_mb={rss_delta_mb} index_bytes={index_bytes} total_bytes={total_bytes} fingerprint={fingerprint:016x}"
    );
    let pass = p95_ms < GATE_P95_MS && recall_at_10 >= GATE_RECALL && rss_delta_mb <= GATE_RSS_MB;
    println!(
        "GATE run=real-fixture p95={p95_ms:.3} ms (< {GATE_P95_MS}) recall@10={recall_at_10:.4} (>= {GATE_RECALL}) rss_delta={rss_delta_mb} MB (<= {GATE_RSS_MB}) → {}",
        if pass { "PASS" } else { "FAIL" }
    );
}
