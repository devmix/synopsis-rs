//! S3b spike: LanceDB ANN measurements on a synthetic 250K×1024 f32 fixture (change
//! native-seam-spikes task 3.2; design D2). Lance is the SOLE ANN engine candidate after
//! usearch (task 3.1) was cancelled by human decision 2026-08-18, so there is no
//! cross-engine comparison — this spike produces the metrics and feeds ADR 0003.
//!
//! Protocol (inherited from Revision 1 of cancelled task 3.1; binding):
//!   - Seeded synthetic corpus: N = 250_000 × DIM = 1024, f32, L2-normalized, fixed embedded
//!     SEED — identical across runs and machines (SplitMix64 + Box–Muller). Geometry is a
//!     256-cluster Gaussian mixture on the unit sphere (modest intra-cluster structure, like
//!     real embedding corpora; the synthetic-geometry caveat is carried into ADR 0003 per D2).
//!   - Index: LanceDB IVF-HNSW with M = 16 and efConstruction = 100 (protocol), quantization
//!     sweep {f32, i8}: f32 → `IvfHnswFlat` (raw vectors + HNSW graph per partition),
//!     i8 → `IvfHnswSq` (u8 scalar-quantized vectors + HNSW graph, 4x compression). Fixed
//!     across all configs: IVF num_partitions = 256, nprobes = 32, L2 metric.
//!   - Queries: 100 held-out vectors (generated after the corpus, never inserted), top-k = 50,
//!     efSearch sweep {50, 200}; warmup 10 queries discarded per config; p50/p95 over the rest.
//!   - Ground truth: exact brute-force L2² top-50 over the full corpus, computed in this spike
//!     (parallel); recall@10 = mean over queries of |ANN-top-10 ∩ exact-top-10| / 10.
//!   - Per-config metrics: p50/p95 latency, peak RSS delta during the search phase, on-disk size.
//!   - Gates (openspec/config.yaml): p95 < 10 ms AND recall@10 >= 0.95 AND RSS delta <= ~2 GB
//!     → GO material for ADR 0003; otherwise NO-GO + escalation (task acceptance criteria).
//!
//! Usage (from repo root) — RELEASE BUILD IS MANDATORY (debug timings are meaningless and
//! orders of magnitude slower, per protocol):
//!   cargo run --release -p spikes --bin s3b_lance [--work-dir <dir>]
//! Search-only diagnostic mode (re-measure the search phase against an index already built in
//! this work dir by a full run; no rebuild, work dir NOT cleared — for isolating IVF coverage):
//!   cargo run --release -p spikes --bin s3b_lance -- \
//!       --work-dir <dir> --reuse <f32_ef50|f32_ef200|i8_ef50|i8_ef200> --nprobes <N> --ef <E>
//! The work dir (default /tmp/opencode/s3b-lance, ~6 GB for the 4 configs) is cleared at start
//! in full mode only. On hosts where /tmp is a small tmpfs and RAM is shared, pass a
//! disk-backed --work-dir explicitly.
//! Every run prints machine-readable `METRIC ...` lines so two runs can be diffed for the
//! <5% reproducibility gate; a corpus fingerprint line proves generation determinism.

use std::f64::consts::PI;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use futures::StreamExt;
use lancedb::arrow::arrow_array::{FixedSizeListArray, Float32Array, Int64Array, RecordBatch};
use lancedb::arrow::arrow_schema::{DataType as ArrowType, Field, Schema as ArrowSchema};
use lancedb::index::{
    Index,
    vector::{IvfHnswFlatIndexBuilder, IvfHnswSqIndexBuilder},
};
use lancedb::query::{ExecutableQuery, QueryBase, Select};
use lancedb::{DistanceType, Table, connect};

// ── Protocol constants (task 3.2 body + Revision 1 of cancelled task 3.1) ────────────────

/// Corpus size: N = 250K vectors (reduced from 1M per human decision; the full-1M run stays
/// with the `vectors` module change, mitigation D2).
const N: usize = 250_000;
/// Vector dimension (bge-m3 registry dim).
const DIM: usize = 1024;
/// Held-out query count.
const N_QUERIES: usize = 100;
/// ANN top-k per query.
const TOP_K: usize = 50;
/// Recall depth for the acceptance metric (recall@10).
const RECALL_AT: usize = 10;
/// Warmup queries discarded before timing, per config.
const WARMUP: usize = 10;
/// Embedded seed — reproducible corpus and held-out queries across runs/machines.
const SEED: u64 = 0x5EED_3B25_CAFE_BAB7;
/// Cluster count of the Gaussian-mixture geometry (modest structure, realistic for embeddings).
const N_CLUSTERS: usize = 256;
/// Per-dimension noise sigma added to a cluster center before re-normalization.
const NOISE_SIGMA: f64 = 0.1;
/// HNSW M (protocol value {16}).
const HNSW_M: u32 = 16;
/// HNSW efConstruction (protocol value, single sweep point {100}).
const EF_CONSTRUCTION: u32 = 100;
/// IVF partitions, fixed across all configs (lancedb default is sqrt(N) ≈ 500; we pin it).
const NUM_PARTITIONS: u32 = 256;
/// Partitions probed per query, fixed across all configs.
const NPROBES: usize = 32;
/// efSearch sweep — key decision parameter carried over from the cancelled usearch protocol.
const EFS: [usize; 2] = [50, 200];
/// 2^-53 as a decimal literal (Rust hex-float literals require a fractional part, so `0x1p-53`
/// is not accepted by the parser).
const INV_2_POW_53: f64 = 1.1102230246251565e-16;
/// FNV-1a 64-bit prime (2^40 + 2^8 + 0xB3) for the corpus fingerprint.
const FNV64_PRIME: u64 = 0x0000_0100_0000_01B3;
/// Arrow batch size for corpus writing (bounds transient memory during table creation).
const BATCH_ROWS: usize = 50_000;

/// Vector column name in the LanceDB table (the table dir is `<name>.lance`).
const COL_VECTOR: &str = "vector";
/// Row id column returned by searches.
const COL_ID: &str = "id";
/// Distance column added to vector-search results by lancedb/lance.
const COL_DIST: &str = "_distance";

// ── Gates (openspec/config.yaml context, hard constraints) ───────────────────────────────

/// p95 search latency gate in milliseconds.
const GATE_P95_MS: f64 = 10.0;
/// recall@10 gate.
const GATE_RECALL: f64 = 0.95;
/// Peak-RSS-delta gate during the search phase, MB (~2 GB).
const GATE_RSS_MB: u64 = 2048;

/// One measured configuration: quantization family × efSearch value (4 total).
struct Cfg {
    /// Stable key used for work-dir subdirs and METRIC lines.
    key: &'static str,
    /// true → i8 scalar-quantized (IvfHnswSq); false → f32 flat (IvfHnswFlat).
    quant_i8: bool,
    /// efSearch beam width for this config.
    ef: usize,
}

const CONFIGS: [Cfg; 4] = [
    Cfg {
        key: "f32_ef50",
        quant_i8: false,
        ef: EFS[0],
    },
    Cfg {
        key: "f32_ef200",
        quant_i8: false,
        ef: EFS[1],
    },
    Cfg {
        key: "i8_ef50",
        quant_i8: true,
        ef: EFS[0],
    },
    Cfg {
        key: "i8_ef200",
        quant_i8: true,
        ef: EFS[1],
    },
];

/// Per-config measurement record (full numbers are emitted as METRIC lines; this struct holds
/// what the final gate table evaluates).
struct Report {
    /// Config key (protocol keys like `f32_ef50`, or diagnostic names like `i8_ef50_np256_ef200`).
    cfg: String,
    p50_ms: f64,
    p95_ms: f64,
    recall_at_10: f64,
    rss_delta_mb: u64,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    if let Err(e) = run(std::env::args().skip(1).collect()).await {
        eprintln!("s3b_lance: FAIL — {e}");
        std::process::exit(1);
    }
}

/// Deterministic SplitMix64 PRNG with a cached Box–Muller spare. The fixed SEED makes the
/// corpus and held-out queries byte-identical across runs, machines and toolchains (pure
/// integer + IEEE-754 ops; no platform-dependent randomness anywhere in generation).
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

    /// Uniform integer in [0, n) (modulo bias is irrelevant for cluster assignment).
    fn uniform_below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// Generated fixture: flat corpus rows + held-out queries.
struct Fixture {
    /// N × DIM f32, row-major; each row L2-normalized at generation time.
    flat: Vec<f32>,
    /// N_QUERIES held-out unit vectors, never inserted into any index.
    queries: Vec<Vec<f32>>,
}

/// One mixture draw: normalize(center + sigma * gauss), returned as an f32 row.
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

/// Generate the full fixture from SEED: N corpus rows then N_QUERIES held-out queries, all
/// drawn sequentially from one PRNG stream (queries are held out by construction — they are
/// never written to any table).
fn generate_fixture() -> Fixture {
    let mut rng = Rng::new(SEED);

    // Cluster centers: random unit vectors in f64.
    let mut centers: Vec<Vec<f64>> = Vec::with_capacity(N_CLUSTERS);
    for _ in 0..N_CLUSTERS {
        let v: Vec<f64> = (0..DIM).map(|_| rng.gauss()).collect();
        let n = v.iter().map(|x| x * x).sum::<f64>().sqrt();
        centers.push(v.into_iter().map(|x| x / n).collect());
    }

    let mut flat: Vec<f32> = Vec::with_capacity(N * DIM);
    for _ in 0..N {
        flat.extend(draw_point(&mut rng, &centers));
    }

    let queries: Vec<Vec<f32>> = (0..N_QUERIES)
        .map(|_| draw_point(&mut rng, &centers))
        .collect();
    Fixture { flat, queries }
}

/// Exact top-50 by L2² for one query against the full corpus (ids only — recall needs sets,
/// not distances).
struct ExactTop {
    /// Sorted ascending by exact distance; len == TOP_K.
    ids: Vec<u32>,
}

/// Brute-force ground truth for all held-out queries (parallel over workers). L2² is computed
/// by direct squared-difference accumulation — the same metric family as the index's L2.
fn brute_force_all(fix: &Fixture, workers: usize) -> Vec<ExactTop> {
    std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(workers);
        for w in 0..workers {
            let start = (w * N_QUERIES) / workers;
            let end = ((w + 1) * N_QUERIES) / workers;
            if start >= end {
                continue;
            }
            handles.push(s.spawn(move || {
                let mut out = Vec::with_capacity(end - start);
                for q in &fix.queries[start..end] {
                    // Full (distance, id) list per query (~4 MB), sorted once — f64 has no Ord
                    // impl, so a BinaryHeap of pairs is not usable; total_cmp makes the order
                    // deterministic even on exact ties. Cost is dominated by the dot product.
                    let mut all: Vec<(f64, u32)> = Vec::with_capacity(N);
                    for i in 0..N {
                        let row = &fix.flat[i * DIM..(i + 1) * DIM];
                        let mut acc: f32 = 0.0;
                        for (a, b) in q.iter().zip(row.iter()) {
                            let d = a - b;
                            acc += d * d;
                        }
                        all.push((acc as f64, i as u32));
                    }
                    all.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
                    let res: Vec<(f64, u32)> = all.into_iter().take(TOP_K).collect();
                    out.push(ExactTop {
                        ids: res.iter().map(|(_, i)| *i).collect(),
                    });
                }
                out
            }));
        }
        let mut out = Vec::with_capacity(N_QUERIES);
        for h in handles {
            match h.join() {
                Ok(v) => out.extend(v),
                Err(_) => std::process::abort(), // a worker panic means broken fixture access; fail hard
            }
        }
        debug_assert_eq!(out.len(), N_QUERIES);
        out
    })
}

/// Build the Arrow RecordBatches that carry the corpus into LanceDB (id + fixed-size-list vector).
fn corpus_batches(flat: &[f32]) -> Result<Vec<RecordBatch>, String> {
    let item_field = std::sync::Arc::new(Field::new("item", ArrowType::Float32, true));
    let schema = std::sync::Arc::new(ArrowSchema::new(vec![
        Field::new(COL_ID, ArrowType::Int64, false),
        Field::new(
            COL_VECTOR,
            ArrowType::FixedSizeList(item_field.clone(), DIM as i32),
            false,
        ),
    ]));

    let mut batches = Vec::with_capacity(N / BATCH_ROWS);
    for (b, chunk) in flat.chunks(BATCH_ROWS * DIM).enumerate() {
        let first_id = b as i64 * BATCH_ROWS as i64;
        let ids = Int64Array::from_iter_values(first_id..first_id + (chunk.len() / DIM) as i64);
        // arrow-array 58 has no from_slice for primitive arrays; the per-batch copy is ~200 MB
        // and transient (the source `flat` stays alive for all four configs).
        let values = Float32Array::from(chunk.to_vec());
        let list = FixedSizeListArray::try_new(
            item_field.clone(),
            DIM as i32,
            std::sync::Arc::new(values),
            None,
        )
        .map_err(|e| format!("build fixed-size-list batch {b}: {e}"))?;
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![std::sync::Arc::new(ids), std::sync::Arc::new(list)],
        )
        .map_err(|e| format!("build record batch {b}: {e}"))?;
        batches.push(batch);
    }
    Ok(batches)
}

/// IVF-HNSW index with the protocol HNSW parameters (M=16, efConstruction=100), f32 flat.
fn flat_index() -> Index {
    Index::IvfHnswFlat(
        IvfHnswFlatIndexBuilder::default()
            .num_partitions(NUM_PARTITIONS)
            .num_edges(HNSW_M)
            .ef_construction(EF_CONSTRUCTION)
            .distance_type(DistanceType::L2),
    )
}

/// IVF-HNSW index with the protocol HNSW parameters, u8 scalar quantization (protocol "i8").
fn sq_index() -> Index {
    Index::IvfHnswSq(
        IvfHnswSqIndexBuilder::default()
            .num_partitions(NUM_PARTITIONS)
            .num_edges(HNSW_M)
            .ef_construction(EF_CONSTRUCTION)
            .distance_type(DistanceType::L2),
    )
}

/// Run one configuration end-to-end: fresh table → index build → search phase (warmup + timed).
async fn run_config(
    cfg: &Cfg,
    fix: &Fixture,
    exact: &[ExactTop],
    work_dir: &Path,
) -> Result<Report, String> {
    let db_dir = work_dir.join(cfg.key);

    // 1) Write the corpus as a fresh table (each config gets its own directory → independent index).
    let t0 = Instant::now();
    fs::create_dir_all(&db_dir).map_err(|e| format!("create {}: {e}", db_dir.display()))?;
    let conn = connect(db_dir.to_str().ok_or("non-UTF8 work dir")?)
        .execute()
        .await
        .map_err(|e| format!("connect {}: {e}", db_dir.display()))?;
    let table: Table = conn
        .create_table(COL_VECTOR, corpus_batches(&fix.flat)?)
        .execute()
        .await
        .map_err(|e| format!("create table in {}: {e}", cfg.key))?;
    let rows = table
        .count_rows(None)
        .await
        .map_err(|e| format!("count rows: {e}"))?;
    if rows != N {
        return Err(format!("row count {} != expected {N} in {}", rows, cfg.key));
    }
    let write_ms = t0.elapsed().as_millis();

    // 2) Build the IVF-HNSW index (quantization family per config).
    let t0 = Instant::now();
    table
        .create_index(
            &[COL_VECTOR],
            if cfg.quant_i8 {
                sq_index()
            } else {
                flat_index()
            },
        )
        .execute()
        .await
        .map_err(|e| format!("build IVF-HNSW index for {}: {e}", cfg.key))?;
    let build_ms = t0.elapsed().as_millis();

    // 3) On-disk sizes (total table dir + _indices subtotal).
    let (total_bytes, index_bytes) = dir_sizes(&db_dir);

    // 4) Search phase: RSS baseline → warmup (untimed) → timed queries with per-query RSS sampling.
    let (p50_ms, p95_ms, recall_at_10, rss_delta_mb) =
        measure_search(&table, fix, exact, cfg.ef, NPROBES).await?;

    println!(
        "METRIC cfg={} quant={} ef={} write_ms={} build_ms={} search_p50_ms={:.3} search_p95_ms={:.3} recall_at_10={:.4} rss_delta_mb={} index_bytes={} total_bytes={}",
        cfg.key,
        if cfg.quant_i8 { "i8" } else { "f32" },
        cfg.ef,
        write_ms,
        build_ms,
        p50_ms,
        p95_ms,
        recall_at_10,
        rss_delta_mb,
        index_bytes,
        total_bytes
    );

    Ok(Report {
        cfg: cfg.key.to_string(),
        p50_ms,
        p95_ms,
        recall_at_10,
        rss_delta_mb,
    })
}

/// Search phase against a built index: RSS baseline → WARMUP untimed queries (cache/index-path
/// warmup) → N_QUERIES timed. Returns (p50 ms, p95 ms, recall@RECALL_AT vs the exact ground
/// truth, peak RSS delta MB during the phase). The same function serves full runs and the
/// search-only diagnostic mode, so both measure an identical code path.
async fn measure_search(
    table: &Table,
    fix: &Fixture,
    exact: &[ExactTop],
    ef: usize,
    nprobes: usize,
) -> Result<(f64, f64, f64, u64), String> {
    let rss_base_kb = vm_rss_kb()?;
    for q in &fix.queries[..WARMUP] {
        run_search(table, q, ef, nprobes).await?; // discarded: warms caches and the index path
    }
    let mut lat_us: Vec<f64> = Vec::with_capacity(N_QUERIES);
    let mut hits: u32 = 0;
    let mut rss_max_kb = rss_base_kb;
    for (qi, q) in fix.queries.iter().enumerate() {
        let t0 = Instant::now();
        // The stream row order is not guaranteed by the SDK docs, so re-sort explicitly.
        let mut hits_rows = run_search(table, q, ef, nprobes).await?;
        lat_us.push(t0.elapsed().as_secs_f64() * 1e6);
        rss_max_kb = rss_max_kb.max(vm_rss_kb()?);
        hits_rows.sort_by(|a, b| a.1.total_cmp(&b.1));

        // recall@RECALL_AT: ANN top-10 vs the exact top-10 set (contains — the exact ids are
        // in distance order, NOT value order, so binary_search would be wrong here).
        for (id, _) in hits_rows.iter().take(RECALL_AT) {
            if exact[qi].ids[..RECALL_AT].contains(id) {
                hits += 1;
            }
        }
    }

    let (p50_ms, p95_ms) = percentile_pair(&mut lat_us);
    let recall_at_10 = hits as f64 / (N_QUERIES * RECALL_AT) as f64;
    Ok((
        p50_ms,
        p95_ms,
        recall_at_10,
        rss_max_kb.saturating_sub(rss_base_kb) / 1024,
    ))
}

/// One indexed vector search: returns (id, distance) rows in returned (distance-ascending) order.
async fn run_search(
    table: &Table,
    q: &[f32],
    ef: usize,
    nprobes: usize,
) -> Result<Vec<(u32, f32)>, String> {
    let mut stream = table
        .vector_search(q)
        .map_err(|e| format!("build vector search: {e}"))?
        .select(Select::columns(&[COL_ID]))
        .ef(ef)
        .nprobes(nprobes)
        .limit(TOP_K)
        .execute()
        .await
        .map_err(|e| format!("execute vector search (ef={ef}): {e}"))?;

    let mut out: Vec<(u32, f32)> = Vec::with_capacity(TOP_K);
    while let Some(batch) = stream.next().await {
        let batch = batch.map_err(|e| format!("read result batch: {e}"))?;
        let ids = batch
            .column_by_name(COL_ID)
            .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
            .ok_or_else(|| "result batch lacks the id column".to_string())?;
        let dists = batch
            .column_by_name(COL_DIST)
            .and_then(|c| c.as_any().downcast_ref::<Float32Array>())
            .ok_or_else(|| "result batch lacks the _distance column".to_string())?;
        for (id, d) in ids.iter().zip(dists.iter()) {
            if let Some(idv) = id {
                out.push((idv as u32, d.unwrap_or(0.0)));
            }
        }
    }
    Ok(out)
}

/// VmRSS of this process in kB (Linux /proc/self/status). The spike is Linux-only by design
/// (laptop target), so a missing file is a hard failure rather than an "n/a".
fn vm_rss_kb() -> Result<u64, String> {
    let status = fs::read_to_string("/proc/self/status")
        .map_err(|e| format!("read /proc/self/status: {e}"))?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .trim()
                .trim_end_matches(" kB")
                .parse::<u64>()
                .map_err(|e| format!("parse VmRSS: {e}"));
        }
    }
    Err("VmRSS not found in /proc/self/status".to_string())
}

/// (total bytes under dir, bytes under `<dir>/<table>.lance/_indices`) — recursive walk.
fn dir_sizes(db_dir: &Path) -> (u64, u64) {
    let mut total = 0_u64;
    let mut index_part = 0_u64;
    let indices_prefix = format!("{COL_VECTOR}.lance/_indices/");
    walk(db_dir, &mut |p| {
        if p.is_file() {
            let len = fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            total += len;
            if p.to_string_lossy().contains(&indices_prefix) {
                index_part += len;
            }
        }
    });
    (total, index_part)
}

/// Recursive file walk (std-only, no extra dependency).
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

/// Nearest-rank p50/p95 of a latency sample (microseconds in, milliseconds out). The sample is
/// always non-empty (N_QUERIES timed queries), and the computed rank index stays in [0, n).
fn percentile_pair(lat_us: &mut [f64]) -> (f64, f64) {
    lat_us.sort_by(|a, b| a.total_cmp(b));
    let n = lat_us.len();
    let at =
        |p: usize| lat_us[usize::min(n, ((n as f64 * p as f64 / 100.0).ceil()) as usize) - 1] / 1e3;
    (at(50), at(95))
}

/// FNV-1a over the corpus bytes — cheap deterministic fingerprint for cross-run evidence
/// (native-endian f32 byte pattern; same machine across runs, which is what we compare).
fn corpus_fingerprint(flat: &[f32]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for x in flat {
        for byte in x.to_ne_bytes() {
            h ^= byte as u64;
            h = h.wrapping_mul(FNV64_PRIME);
        }
    }
    h
}

/// Drive the spike: fixture → ground truth → 4 configs (full mode) or a search-only diagnostic
/// against an existing index (`--reuse`) → gate table.
async fn run(args: Vec<String>) -> Result<(), String> {
    let mut work_dir = PathBuf::from("/tmp/opencode/s3b-lance");
    let mut reuse_key: Option<&'static str> = None;
    let mut nprobes_opt: Option<usize> = None;
    let mut ef_opt: Option<usize> = None;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--work-dir" => {
                let v = it.next().ok_or("--work-dir needs a value")?;
                work_dir = PathBuf::from(v);
            }
            // Search-only mode: open the existing table/index of one config (built by an earlier
            // full run in this same work dir) and re-measure the search phase with explicit
            // nprobes/ef — used to isolate IVF coverage effects without rebuilding indexes.
            "--reuse" => {
                let v = it.next().ok_or("--reuse needs a value")?;
                reuse_key = CONFIGS.iter().find(|c| c.key == v).map(|c| c.key);
                if reuse_key.is_none() {
                    return Err(format!("--reuse: unknown config key '{v}'"));
                }
            }
            "--nprobes" => {
                let v = it.next().ok_or("--nprobes needs a value")?;
                nprobes_opt = Some(v.parse().map_err(|_| format!("bad --nprobes '{v}'"))?);
            }
            "--ef" => {
                let v = it.next().ok_or("--ef needs a value")?;
                ef_opt = Some(v.parse().map_err(|_| format!("bad --ef '{v}'"))?);
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    if reuse_key.is_some() && (nprobes_opt.is_none() || ef_opt.is_none()) {
        return Err("--reuse mode requires both --nprobes <N> and --ef <E>".to_string());
    }

    if cfg!(debug_assertions) {
        // Protocol: release build mandatory (the dev profile has debug-assertions on; the
        // `release` cfg name itself is not cargo-declared and would trip unexpected_cfgs).
        return Err("dev build: run `cargo run --release -p spikes --bin s3b_lance`".to_string());
    }

    if reuse_key.is_none() && work_dir.exists() {
        fs::remove_dir_all(&work_dir).map_err(|e| format!("clear {}: {e}", work_dir.display()))?;
    }

    // 1) Fixture (deterministic from SEED).
    let t0 = Instant::now();
    let fix = generate_fixture();
    println!(
        "info: generated {}×{} corpus + {} held-out queries in {} ms (seed {SEED:#x}, mixture K={N_CLUSTERS} sigma={NOISE_SIGMA})",
        N,
        DIM,
        N_QUERIES,
        t0.elapsed().as_millis()
    );
    println!(
        "FINGERPRINT corpus_fnv1a={:016x} first_query_head=[{}]",
        corpus_fingerprint(&fix.flat),
        fix.queries[0]
            .iter()
            .take(4)
            .map(|x| format!("{x:.6}"))
            .collect::<Vec<_>>()
            .join(", ")
    );

    // 2) Brute-force ground truth (parallel; not part of the timed ANN path).
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(N_QUERIES);
    let t0 = Instant::now();
    let exact = brute_force_all(&fix, workers);
    println!(
        "info: brute-force ground truth (exact L2² top-{TOP_K} × {N_QUERIES} queries over {}×{} with {workers} workers) took {} ms",
        N,
        DIM,
        t0.elapsed().as_millis()
    );

    // 3) Measurement: the four protocol configs (full mode), or a search-only diagnostic on an
    // existing index (--reuse; write/build stages skipped and reported as 0).
    let mut reports = Vec::with_capacity(CONFIGS.len());
    if let (Some(key), Some(nprobes), Some(ef)) = (reuse_key, nprobes_opt, ef_opt) {
        // key comes from CONFIGS itself, so the lookup is guaranteed to hit.
        let cfg = CONFIGS
            .iter()
            .find(|c| c.key == key)
            .ok_or("internal: reuse key")?;
        let db_dir = work_dir.join(key);
        println!(
            "info: search-only mode — reusing existing index in {} with nprobes={nprobes} ef={ef}",
            db_dir.display()
        );
        let conn = connect(db_dir.to_str().ok_or("non-UTF8 work dir")?)
            .execute()
            .await
            .map_err(|e| format!("connect {}: {e}", db_dir.display()))?;
        let table: Table = conn
            .open_table(COL_VECTOR)
            .execute()
            .await
            .map_err(|e| format!("open table in {}: {e}", cfg.key))?;
        let rows = table
            .count_rows(None)
            .await
            .map_err(|e| format!("count rows: {e}"))?;
        if rows != N {
            return Err(format!("row count {} != expected {N} in {}", rows, cfg.key));
        }
        let (total_bytes, index_bytes) = dir_sizes(&db_dir);
        let (p50_ms, p95_ms, recall_at_10, rss_delta_mb) =
            measure_search(&table, &fix, &exact, ef, nprobes).await?;
        let cfg_name = format!("{key}_np{nprobes}_ef{ef}");
        println!(
            "METRIC cfg={} quant={} ef={} write_ms=0 build_ms=0 search_p50_ms={:.3} search_p95_ms={:.3} recall_at_10={:.4} rss_delta_mb={} index_bytes={} total_bytes={}",
            cfg_name,
            if cfg.quant_i8 { "i8" } else { "f32" },
            ef,
            p50_ms,
            p95_ms,
            recall_at_10,
            rss_delta_mb,
            index_bytes,
            total_bytes
        );
        reports.push(Report {
            cfg: cfg_name,
            p50_ms,
            p95_ms,
            recall_at_10,
            rss_delta_mb,
        });
    } else {
        for cfg in &CONFIGS {
            reports.push(run_config(cfg, &fix, &exact, &work_dir).await?);
        }
    }

    // 4) Gate table (openspec/config.yaml gates).
    println!(
        "\nGATES: p95 < {GATE_P95_MS} ms, recall@10 >= {GATE_RECALL}, RSS-delta <= ~{GATE_RSS_MB} MB"
    );
    // A GO decision needs at least one passing configuration (engine + config is what the ADR
    // records); every other passing config is an alternative worth listing in the appendix.
    let mut passing: Vec<String> = Vec::new();
    for r in &reports {
        let ok = r.p95_ms < GATE_P95_MS
            && r.recall_at_10 >= GATE_RECALL
            && r.rss_delta_mb <= GATE_RSS_MB;
        if ok {
            passing.push(r.cfg.clone());
        }
        println!(
            "GATE {} p50={:.3} ms p95={:.3} ms recall@10={:.4} rss_delta={} MB → {}",
            r.cfg,
            r.p50_ms,
            r.p95_ms,
            r.recall_at_10,
            r.rss_delta_mb,
            if ok { "PASS" } else { "FAIL" }
        );
    }
    if passing.is_empty() {
        println!(
            "VERDICT NO-GO-candidate: no config passes the gates (decision recorded in docs/adr/0003-ann-engine.md)"
        );
    } else {
        println!(
            "VERDICT GO-candidate: {} config(s) pass all gates — [{}] (decision recorded in docs/adr/0003-ann-engine.md)",
            passing.len(),
            passing.join(", ")
        );
    }
    Ok(())
}
