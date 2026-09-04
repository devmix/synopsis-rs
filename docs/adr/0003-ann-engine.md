# ADR 0003 — ANN engine: LanceDB (IVF-HNSW, u8-SQ)

**Status:** Superseded by ADR 0004 (2026-08-31): the lance engine was removed; usearch is the sole ANN engine.

**Status:** GO; the configuration is fixed below. Full measurements tables — in [spike-s3-results.md](spike-s3-results.md).
**Date:** 2026-08-18 · **Change:** native-seam-spikes, task 3.2 (spike S3b: `crates/spikes/src/bin/s3b_lance.rs`)

## Question

Which ANN engine and configuration to freeze for the `vectors` crate under the laptop's hard gates (16 GB RAM, one local binary): **p95 < 10 ms**, **recall@10 ≥ 0.95**, **RSS-delta ≤ ~2 GB** on an N=1M×1024-class corpus? The candidate is singular — **lancedb** (the usearch spike was cancelled by human decision on 2026-08-18: hours of run time and an unsatisfactory result).

## Options

Protocol 3.2 (per revision 1): seeded synthetic N=250K×1024 f32, M=16, efConstruction=100, IVF num_partitions=256, nprobes=32, L2, 100 held-out queries, top-k=50, recall@10 against exact-L2 brute force inside the spike. Two key decision parameters are varied — quantization and efSearch:

1. **f32 / IvfHnswFlat** (full precision) × efSearch {50, 200}
2. **i8 / IvfHnswSq** (u8 scalar quantization, ~3× smaller on disk and in RAM) × efSearch {50, 200}

## Measurements (spike s3b_lance, release, two protocol runs A/B + diagnostics; details — appendix)

| cfg | p95 ms (A/B) | recall@10 (A/B) | RSS ΔMB (A/B) | index / total on disk | Gates |
|---|---|---|---|---|---|
| f32_ef50 | 22.0 / 20.4 | 0.902 / 0.899 | 890 / 897 | ~1.06 GB / ~2.09 GB | FAIL (p95, recall) |
| f32_ef200 | 39.7 / 39.3 | 0.994 / 0.990 | 745 / 545 | ~1.06 GB / ~2.09 GB | FAIL (p95) |
| i8_ef50 | 7.66 / 7.77 | 0.885 / 0.884 | 79 / 62 | ~297 MB / ~1.32 GB | FAIL (recall) |
| **i8_ef200** | **9.63 / 9.76** | **0.957 / 0.962** | **71 / 79** | **~297 MB / ~1.32 GB** | **PASS — all three gates, both runs** |

Reproducibility of i8_ef200 (criterion "two runs with <5 % spread"): p50 6.04/6.17 ms (**2.1 %**), p95 9.63/9.76 ms (**1.4 %**) — **met**. Recall is stable across all rebuilds (Δ ≤ 0.005); the corpus fingerprint is identical across all runs (`corpus_fnv1a=50194a997027c6d2`).

Diagnostic sweep nprobes {64,128,256} × efSearch {50,200} (appendix): efSearch dominates recall (ef=50 caps at ~0.88–0.90; ef=200 → 0.96–0.997); raising nprobes from 32→256 gives +1–4 pp recall at the cost of linear p95 growth — no point beats i8_ef200 on the combined gates.

## Decision (GO)

**Engine:** LanceDB, `lancedb = "0.37"` / lance 10.0.0 (Cargo.lock: lancedb 0.37.1, lance 10.0.0). **Configuration for the `vectors` crate:**

| Parameter | Value |
|---|---|
| Index | IVF-HNSW with u8 scalar quantization (`IvfHnswSq`) |
| M (HNSW) | 16 |
| efConstruction | 100 |
| num_partitions (IVF) | 256 |
| nprobes | 32 |
| **efSearch** | **200** (the key parameter: without it recall ~0.88 < the 0.95 gate) |
| Metric | L2 |

Source vectors are stored in the table as f32 (`FixedSizeList<Float32, 1024>`); quantization lives only inside the index; search returns `_distance`, and ranking is by it. Fragmentation: `num_rows_to_batch = 1_000` (the lance default) when writing in a stream via Arrow RecordBatches.

**Footprint (250K×1024):** index ~297 MB, table+index ~1.32 GB on disk; **search RSS-delta ~71–79 MB** — the order of magnitude for N=1M (~300–400 MB RAM with lance's mmap support) fits the laptop budget with a large margin. Disk extrapolation to 1M: ~5.3 GB — acceptable (NVMe).

## Rejected alternatives

- **f32 / IvfHnswFlat** (both efSearch): p95 20–40 ms — 2–4× past the <10 ms gate; recall at ef=50 is below the gate anyway. Full precision is not needed: bge-m3 int8 embeddings are effectively 8-bit in meaning, and i8_ef200 recall passes with a +0.7–1.2 pp margin.
- **i8 / efSearch=50:** p95 7.7 ms — the best timing of all configs, but recall 0.884–0.885 < 0.95 (the gate). efSearch=50 is unacceptable for the production crate at this geometry.
- **Raising nprobes >32** (64/128/256): +1–4 pp recall, but the p95 of i8_ef200 grows 9.7 → 10.3–24.4 ms — past the gate; not needed with efSearch=200.
- **usearch** (cancelled by the human before the spike): hours of run time and an unsatisfactory result — see the revisions of tasks 3.1/3.2.

## Open questions / residual risks

1. **Thin margins at i8_ef200:** p95 9.6–9.8 ms against the <10 ms gate (~3 % headroom) and recall 0.957–0.962 against ≥0.95 (+0.7–1.2 pp). On a more loaded laptop machine p95 may go past the gate. Mitigation in the `vectors` crate: efSearch is a runtime setting (not a constant), default 200; on regression on a real corpus raise nprobes/efSearch or fall back to f32_ef200 at the cost of p95.
2. **Synthetic geometry:** a 256-cluster Gaussian mixture is a question of index mechanics (design D2), but the real bge-m3 corpus may differ. Per the D2 mitigation, the S3 measurements **are repeated on a real fixture** in the `vectors` change (an explicit task there); the full 1M run also stays with it. — *closed by the appendix (2026-08-22): the 1M run was performed — the p95/recall gates are not met, see "Escalation"; the repeat on the real fixture is deferred (the `vectors.bin` export is unavailable).*
3. **Machine noise:** p50/p95 spread >5 % between A/B runs on the NOT-recommended configs (an artifact of a generally loaded machine; both percentiles of i8_ef200 are <5 %) — see the appendix "Reproducibility".
4. **Async measurement path:** the timing includes the full async path search→stream collect through tokio (as in the production crate) — the runtime overhead is accounted for, not attributed to the engine.

## Appendix — full N=1M run (vectors task 1.8, 2026-08-22)

Closure of open question #2 (full scale + repeat on real geometry). Protocol: s3b_lance via `crates/vectors/tests/full_benchmark.rs` (release, `#[ignore]`), ADR 0003 configuration (IvfHnswSq, M=16, efConstruction=100, 256 partitions, nprobes=32, efSearch=200, L2). **lancedb 0.37.1 / lance 10.0.0.** Machine: 16-core laptop, 31 GB RAM, NVMe; **the machine was loaded** (load average ~4–5, shared processes) — see "Reproducibility".

### Run (a): synthetic N=1 000 000×1024

Geometry: a 256-cluster Gaussian mixture on the unit sphere (σ=0.1) — the same family as the spike. The corpus is regenerated line by line (per-line SplitMix64 seed, ~4 GB never resident in RAM), seed `0x5EED_3B25_CAFE_BAB7`, fingerprint `corpus_fnv1a=1d94cb3216d76f01` (A == B across all runs). Ground truth: exact-L2² brute force (f32 accumulation, 16 worker threads, 268–283 s).

| run | write ms | build ms | p50 ms | p95 ms | recall@10 | RSS Δ MB | index / total on disk |
|---|---|---|---|---|---|---|---|
| A | 29 953 | 62 650 | 15.332 | 32.159 | 0.9330 | 1371 | 1.21 GB / 5.35 GB |
| B | 31 358 | 65 763 | 18.624 | 42.866 | 0.9110 | 1343 | 1.21 GB / 5.35 GB |

An additional run the same day (before the sweep): A p50/p95 20.012/55.539 ms, recall 0.9110; B 19.080/40.775 ms, recall 0.9160 — the same order, higher p95 (machine load).

**Gates:** p95 < 10 ms → **FAIL** (32–55 ms, 3–6× past the gate); recall@10 ≥ 0.95 → **FAIL** (0.911–0.933); RSS Δ ≤ ~2 GB → **PASS** (1.0–1.4 GB). Disk: 5.35 GB — matches the ADR extrapolation (~5.3 GB). Index ~1.21 GB (the spike had 0.297 GB at 250K — a 4.1× scale, as expected).

**Reproducibility (gate <5 %):** NOT met — p50 spread 21.5 %, p95 spread 33.3 % (a machine-load artifact; cf. open question #3). Recall between two independent builds of one config: 0.911/0.933 — a build-to-build variance of ~2 pp from the random k-means/HNSW initializations inside lance (the deterministic index files differ in that case).

### Mitigation sweep (runtime parameters, no rebuild; run B's index)

| nprobes | efSearch | p50 ms | p95 ms | recall@10 | RSS Δ MB |
|---|---|---|---|---|---|
| 32 | 200 (ADR default) | 18.624 | 42.866 | 0.9110 | 1343 |
| 64 | 200 | 26.619 | 46.425 | 0.9120 | 1072 |
| 128 | 200 | 41.249 | 66.627 | 0.9120 | 499 |
| 256 | 200 | 72.060 | 89.883 | 0.9120 | 362 |
| 128 | 400 | 59.258 | 72.484 | **0.9600** | 868 |
| 256 | 400 | 115.388 | 141.417 | **0.9600** | 297 |

**Observation:** at N=1M recall is limited by **efSearch** (the HNSW walk inside a partition), not by IVF coverage: nprobes 32→256 does not raise recall (0.911→0.912), while efSearch 200→400 raises it to 0.960. At the same time, the p95 < 10 ms gate is **unachievable at any point** with recall ≥ 0.95 (72–141 ms at ef=400). Contrast with the 250K spike (i8_ef200: p95 9.6 ms, recall 0.957 — all gates PASS): at 4× scale the p95 budget is exhausted.

### Run (b): real vectors.bin fixture

**Deferred** — the `vectors.bin` export is unavailable (task 1.8: NOT a blocker for archiving the change). Harness ready: `SYNOPSIS_BENCH_VECTORS_BIN=<path> cargo test -p vectors --release -- --ignored full_benchmark_real_fixture --nocapture`. Documented deviation: queries — a seeded sample of corpus lines (no held-out set in the export); recall@10 against exact top-10 is still well defined.

### Run commands (reproducibility)

```sh
# run (a): A/B + gates, ~9 min, release only, ~5.4 GB of disk
cargo test -p vectors --release -- --ignored full_benchmark_synthetic --nocapture
# same + mitigation sweep on run B's index
SYNOPSIS_BENCH_SWEEP=1 cargo test -p vectors --release -- --ignored full_benchmark_synthetic --nocapture
# run (b) — once the vectors.bin export becomes available
SYNOPSIS_BENCH_VECTORS_BIN=<vectors.bin> cargo test -p vectors --release -- --ignored full_benchmark_real_fixture --nocapture
# env: SYNOPSIS_BENCH_N (default 1_000_000), SYNOPSIS_BENCH_WORK_DIR (default /mnt/local/sandbox/opencode-vectors-18)
```

### Escalation to the user (deviation from ADR 0003 gates)

At full scale N=1M the ADR 0003 configuration **does not hold the p95 (<10 ms) and recall (≥0.95) gates** on this machine; the RSS (≤2 GB) and disk (5.35 GB) gates are met with margin. No runtime setting (the sweep above) passes both gates at once. Options (the decision is up to the user):

1. **Accept the deviation at 1M** — a real laptop corpus is probably smaller than 1M (at 250K all gates hold with margin); fix the 1M numbers as worst-case behavior.
2. **Rebuild the index with a different configuration** (M / efConstruction / num_partitions) — a separate ADR decision, not a runtime mitigation; requires a repeated full run.
3. **Raise the default efSearch (200→400)** — recall recovers (0.960), but p95 at 1M becomes 72–141 ms — the p95 gate still is not met; justified only by a conscious waiver of the p95 gate on large corpora.
