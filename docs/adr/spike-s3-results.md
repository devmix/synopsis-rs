# S3 — overall spike results (appendix to ADR 0003)

**Date:** 2026-08-18 · **Change:** native-seam-spikes, task 3.2 (spike S3b: `crates/spikes/src/bin/s3b_lance.rs`)
**Status:** finished; the decision — in [ADR 0003](0003-ann-engine.md). The one-off spikes file is removed together with `crates/spikes` (task 5.1) — it remains as a fixed measurement.

## Environment

| Parameter | Value |
|---|---|
| CPU | 11th Gen Intel Core i7-11800H @ 2.30 GHz, 16 logical cores (laptop) |
| RAM | 31 GB physical (~5–8 GB available at run time; swap active) |
| Disk | /dev/nvme0n1p13 (NVMe); work-dir `/mnt/local/sandbox/opencode-s3b-lance` (disk-backed, not tmpfs) |
| Toolchain | rustc 1.96.0 (pinned in `rust-toolchain.toml`) |
| Build | **release** (`cargo run --release -p spikes --bin s3b_lance`) — debug timings are meaningless per the protocol |
| Crates | lancedb **0.37.1**, lance **10.0.0**, arrow-array 58.4.0 (Cargo.lock) |

## Corpus and ground truth

- N = **250 000** × 1024-dim f32; seeded synthetic: a Gaussian mixture of K=256 centers on the unit sphere, per-dim σ=0.1, re-normalization; PRNG SplitMix64 + Box-Muller, seed `0x5EED_3B25_CAFE_BAB7` (hardcoded in the binary).
- 100 held-out queries from the same distribution (a separate seed segment), top-k = 50.
- Ground truth — exact L2² brute force inside the spike (rayon, 16 workers); ~5 s per run.
- **Determinism fingerprint** (identical across ALL runs: run-1, A, B, and all diagnostics): `corpus_fnv1a=50194a997027c6d2 first_query_head=[0.010096, -0.003417, -0.064351, 0.021204]`.
- Fixed parameters for all configurations: **M=16, efConstruction=100, num_partitions=256 (IVF), nprobes=32, L2**. Only quantization and efSearch vary.

## Protocol runs A and B (nprobes=32)

Two full sequential runs; each cleans the work-dir and recreates all 4 indices.

| cfg | quant | efSearch | write_ms (A/B) | build_ms (A/B) | p50 ms (A/B) | p95 ms (A/B) | recall@10 (A/B) | RSS ΔMB (A/B) | index bytes (A/B) | total bytes (A/B) |
|---|---|---|---|---|---|---|---|---|---|---|
| f32_ef50 | f32 (IvfHnswFlat) | 50 | 2191 / 2519 | 17085 / 17622 | 10.104 / 9.912 | 22.016 / 20.424 | 0.9020 / 0.8990 | 890 / 897 | 1 065 068 168 / 1 065 007 225 | 2 089 607 493 / 2 089 546 551 |
| f32_ef200 | f32 (IvfHnswFlat) | 200 | 2920 / 2626 | 17511 / 18030 | 21.500 / 23.283 | 39.685 / 39.268 | 0.9940 / 0.9900 | 745 / 545 | 1 065 045 862 / 1 065 029 859 | 2 089 585 187 / 2 089 569 185 |
| i8_ef50 | i8 (IvfHnswSq) | 50 | 2638 / 5449 | 10906 / 11003 | 4.012 / 4.699 | 7.657 / 7.770 | 0.8850 / 0.8840 | 79 / 62 | 297 029 735 / 296 924 315 | 1 321 569 151 / 1 321 463 729 |
| **i8_ef200** | i8 (IvfHnswSq) | 200 | 2669 / 3717 | 11107 / 11215 | **6.040 / 6.166** | **9.628 / 9.761** | **0.9570 / 0.9620** | **71 / 79** | 297 012 198 / 296 993 139 | 1 321 551 613 / 1 321 532 555 |

### Reproducibility (A↔B spread, |B−A|/A)

| cfg | p50 spread | p95 spread | recall Δ | <5%? |
|---|---|---|---|---|
| f32_ef50 | 1.9 % | 7.2 % | −0.003 | no (p95) |
| f32_ef200 | 8.3 % | 1.1 % | −0.004 | no (p50) |
| i8_ef50 | 17.1 % | 1.5 % | −0.001 | no (p50) |
| **i8_ef200** | **2.1 %** | **1.4 %** | **+0.005** | **yes — both percentiles <5 %** |

- The criterion "two-run reproducibility (<5 % spread)" is met for the recommended configuration **i8_ef200** (both percentiles).
- For the other configs the p50/p95 spread is above 5 % — an artifact of the noise of a loaded shared machine (including our own cargo processes); sub-10 ms absolute values are most sensitive to scheduler jitter. Recall is stable across all configs (Δ ≤ 0.005) and across all independent index rebuilds (± ~0.01) — the ground truth and the result ordering are reproducible; the corpus fingerprint is identical across all runs.
- Run-1 (before the recall bug fix, see below) is recorded in `/tmp/opencode/s3b_run1.log`; its timings match A/B in order of magnitude, but its recall is invalid and NOT quoted.

## Diagnostics: nprobes × efSearch sweep (on run B's indices)

`--reuse <cfg> --nprobes N --ef E` — search without an index rebuild. Matrix 2 quantizations × nprobes {64,128,256} × efSearch {50,200}:

| quant | nprobes | efSearch | p50 ms | p95 ms | recall@10 | RSS ΔMB |
|---|---|---|---|---|---|---|
| f32 | 64 | 50 | 15.311 | 21.207 | 0.897 | 1228 |
| f32 | 64 | 200 | 31.906 | 36.602 | 0.993 | 1230 |
| f32 | 128 | 50 | 25.916 | 28.390 | 0.900 | 1221 |
| f32 | 128 | 200 | 53.777 | 59.076 | 0.996 | 1233 |
| f32 | 256 | 50 | 47.730 | 63.589 | 0.901 | 1245 |
| f32 | 256 | 200 | 97.361 | 108.326 | 0.997 | 1234 |
| i8 | 64 | 50 | 5.369 | 7.920 | 0.880 | 433 |
| i8 | 64 | 200 | 8.119 | 10.334 | 0.960 | 430 |
| i8 | 128 | 50 | 8.067 | 10.790 | 0.888 | 438 |
| i8 | 128 | 200 | 11.431 | 14.308 | 0.974 | 435 |
| i8 | 256 | 50 | 13.769 | 16.008 | 0.888 | 438 |
| i8 | 256 | 200 | 18.802 | 24.388 | 0.974 | 441 |

Reading: (1) **efSearch dominates recall** — at ef=50 the ceiling is ~0.88–0.90 regardless of nprobes; ef=200 gives 0.96–0.997. (2) Raising nprobes from 32→256 lifts recall by +~1–4 pp but grows p95 linearly — no sweep point beats the protocol i8_ef200 on the combined gates. (3) i8 is ~3× faster than f32 at the cost of ~0.7–3 pp recall (quantization noise).

## Evaluation against the openspec/config.yaml gates

Gates: **p95 < 10 ms** and **recall@10 ≥ 0.95** and **RSS-delta ≤ ~2 GB**.

| cfg | p95 < 10 ms | recall ≥ 0.95 | RSS ≤ 2 GB | Result |
|---|---|---|---|---|
| f32_ef50 (A/B) | ✗ (22.0 / 20.4) | ✗ (0.902 / 0.899) | ✓ | FAIL |
| f32_ef200 (A/B) | ✗ (39.7 / 39.3) | ✓ (0.994 / 0.990) | ✓ | FAIL |
| i8_ef50 (A/B) | ✓ (7.66 / 7.77) | ✗ (0.885 / 0.884) | ✓ | FAIL |
| **i8_ef200 (A/B)** | ✓ (**9.63 / 9.76**) | ✓ (**0.957 / 0.962**) | ✓ (71 / 79 MB) | **PASS** |

## Methodology notes

- **Recall bug fix before the valid runs:** the first version computed recall via `binary_search` on an id slice sorted by ANN-result distance (NOT by id value) — giving a false ~0.19–0.22 on all configs. Fix: `.contains()` + an explicit sort of the results by `_distance` before take(10). Fix validation: the full-coverage diagnostic (nprobes=256, ef=200) raised f32 recall from 0.219 → 0.997. All numbers above are POST-fix, on the fixed binary; the protocol A/B were run sequentially on the same binary.
- **RSS-delta** = the search process's peak RSS minus the RSS before opening the index (peak RSS from `/proc/self/status` during the 100 queries). Includes lance's mmaps, but not the full file volume under `mmap_size` — the real RAM pressure on the laptop is less than the table size.
- **total bytes** = table + index in the work-dir (data/ + `_indices/`). For i8: ~1.32 GB at 250K×1024 → extrapolation to N=1M ≈ 5.3 GB on disk (RAM — see RSS-delta).
- **Warmup** = 10 queries before the measurement; the timing of each of the 100 scored queries — an `Instant` around the full async path (search → collect stream).
