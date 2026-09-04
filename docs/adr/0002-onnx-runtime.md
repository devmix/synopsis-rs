# ADR 0002 — ONNX Runtime seam in Rust (ort, load-dynamic)

**Status:** GO on the seam; all criteria of task 2.1 are met for ALL THREE models of the onnx.yaml registry (spike `s2_onnx` exits with code 0).
**Date:** 2026-08-18 · **Change:** native-seam-spikes, task 2.1 (spike S2: `crates/spikes/src/bin/s2_onnx.rs`; extended to all 3 registry models — task Revision 2)

## Question

Can a Rust binary own the full "load ONNX Runtime → embeddings for ALL models of the onnx.yaml registry" path without build-time downloads and without external services: dynamic loading of the `.so` from a local `data/` per the onnx.yaml registry, the tokenization/inference pipeline, and deterministic N=50 vectors (dim 1024/384/384) for each of the three models: `bge-m3-int8`, `bge-small-en-v1.5` (the registry and config.default.yaml default), `paraphrase-multilingual-MiniLM-L12-v2`? Which bindings mechanism to freeze for the production `embedding` crate?

## Options

1. **ort (pykeio) 2.x, `load-dynamic` + `api-28`, default-features off** — ORT is neither downloaded nor linked at build time; the `.so` is opened via dlopen at runtime from a path resolved per onnx.yaml. C API level 28 = ORT 1.28 is exactly the version declared by the onnx.yaml registry.
2. **onnxruntime-rs** — the frozen entry from openspec/config.yaml; unresolvable: no crate with that name exists on crates.io (the last publication is a dead 0.2.x against the old API), and the successor `ort` still has no stable release. The pin is fixed as `ort = "2.0.0-rc.13"`.
3. Own FFI over libonnx (hand-written `extern "C"` + raw pointers) — rejected: it duplicates what ort already provides type-safely (Tensor/Session/DynValue) while losing the proven error handling and memory ownership; the rc quality of ort's API is acceptable because the seam is narrow (1 session, 3 tensors).
4. Sidecar process: move embeddings into a separate binary over IPC — rejected: violates the "one local binary" contract, adds per-text process overhead, and makes parity tests dependent on an external process.

## Measurements (spike s2_onnx, release build, run 2026-08-18)

Artifacts of ALL THREE models are preloaded into `<repo>/data/` with a note
(network-download was NOT performed; provenance + sha256 of all 9 files — `data/README.md`). The spike
on every start verifies the size of each file against `size_bytes` from onnx.yaml (all 8 model files +
the runtime library match exactly) and the version string "1.28.0" inside the `.so` (byte scan) — PASS.

Registry: the spike constants (runtime version, `default`, names/size_bytes of all files of the three models)
are verified against the text of `configs/onnx.yaml` on every run start — PASS. Runtime
loading: `ort::init_from(<abs path>)` → dlopen from `data/`, no build-time downloads
(default-features off); initialization <5 ms with a warm page cache.

Per model: 50 fixed texts (EN/RU/empty string/text >512 tokens), batch size 1,
input/output pipeline — own, per the graph's declared I/O (tensor names are taken
from the graph declaration). Measurements table:

| Model | dim | declared inputs → chosen output | ms/text (50 items) | max ‖v‖−1 | peak RSS\* | cross-run\*\* |
|---|---|---|---|---|---|---|
| `bge-m3-int8` | 1024 | `[input_ids, attention_mask]` → `sentence_embedding` (outputs: token_embeddings, sentence_embedding) | **1016.2** (50 809 ms; warm-up 1040 ms) | 5.8e-8 ✓ | 3 125.7 MB (VmPeak 3 200 732 kB) | identical, sha `888490b676…`, 639 415 B |
| `bge-small-en-v1.5` (**default**) | 384 | `[input_ids, attention_mask, token_type_ids]` → `last_hidden_state` (CLS vector [0,0,:]) | **115.6** (5 779 ms; warm-up 120 ms) | 5.86e-8 ✓ | ~800 MB (VmPeak 818 056 / 820 360 kB) | identical, sha `71321cf3…`, 237 964 B |
| `paraphrase-multilingual-MiniLM-L12-v2` | 384 | `[input_ids, attention_mask, token_type_ids]` → `last_hidden_state` (CLS vector [0,0,:]) | **117.7** (5 887 ms; warm-up 126 ms) | 5.18e-8 ✓ | ~2 439 MB (VmPeak 2 499 032 / 2 497 240 kB) | identical, sha `53f90d85…`, 240 110 B |

All vectors are finite; unit-norm within eps 1e-5 (L2 normalization in f64).
Session creation: bge-m3-int8 1535 ms, bge-small-en-v1.5 165 ms, MiniLM-L12 411 ms (tokenizer.json:
557/16/520 ms respectively).

- Determinism: per model, a second pass IN THE SAME PROCESS is bit-identical (50/50) — design D5. **cross-run** — two independent processes (`--model <m> --out`) produced byte-identical JSONL for all three models (cmp clean). ORT CPU inference on this machine is deterministic — storing reference vectors in fixtures is acceptable.
- bge-m3-int8: the sha256 of the dump `888490b676…c463d998` matches the reference recorded before the spike was extended to 3 models → generalizing the pipeline did not change the computation bit-for-bit.
- Running all three models SEQUENTIALLY in one process (`s2_onnx` without `--model`) — PASS on all three; process VmPeak (accumulated) 3125.7 MB, dominated by the int8 weights of bge-m3 (~2.3 GB).
- \* peak RSS = VmPeak of the dedicated single-process `--model <m>` run (the true model peak; two independent runs gave close values — see the table). In a multi-model process, end-of-phase VmRSS is stably ≈1506 MB (the bge-m3 weights remain resident after the session is dropped).
- \*\* cross-run: `cmp` of two JSONL dumps from independent processes; sha256 is given for the first dump.

## Decision (GO)

The seam is proven: Rust owns the full embeddings path through ORT 1.28, loaded dynamically from a local `data/`. **Verdict: GO** — the `embedding` crate is built on:

- bindings pin: `ort = { version = "2.0.0-rc.13", default-features = false, features = ["load-dynamic", "api-28"] }`;
- artifact mechanism — the onnx.yaml registry (download URL + size_bytes + sha256 verify); storage path and resolve logic — `data/` inside the repo (see data/README.md);
- batch size = 1 per text (batching — a task for the production `ingestion` crate, see "Open questions");
- pipeline: `encode(text, add_special_tokens=false)` → truncate to 512 → right-pad id **0** (padID stays zero — a tokenizer.json anomaly) → `attention_mask = [1]×512` → input tensors bound in canonical order `input_ids, attention_mask, token_type_ids` only if declared by the graph (bge-m3-int8 has no token_type_ids; the other two models do) → output by priority: `sentence_embedding` (bge-m3-int8), else `last_hidden_state`, else the single declared output (CLS vector [0,0,:] for last_hidden_state) → L2 normalization in f64, cast back to f32.

### Bindings pin deviation (explanation, ADR 0001 precedent)

The frozen entry "onnxruntime-rs" in openspec/config.yaml is **unresolvable** verbatim: no crate with that name exists on crates.io. The real pin — `ort` (successor, pykeio), version 2.0.0-rc.13 as the only one resolvable against C API level 28 (= ORT 1.28). The intent of the frozen entry (ORT 1.28, dynamic loading, no build-time downloads) is fully preserved; the config.yaml text is not edited — the deviation is recorded here and in the task 2.1 revision.

## Rejected alternatives (see "Options")

- **onnxruntime-rs** — does not exist on crates.io (point 2).
- **Own FFI** — duplicates ort's functionality with no gain; the narrow seam makes the rc API acceptable (point 3).
- **Sidecar process for embeddings** — violates "one local binary" and the parity contract (point 4).

## Open questions (passed to subsequent tasks)

- **~1 s/text (bge-m3-int8) at batch size 1; ~0.12 s/text for the two 384-dim models.** The query path does NOT load the embeddings model — a hard constraint of openspec/config.yaml (context, "Hard constraints"), so the latency only affects ingestion/reindex; the production `ingestion` crate must batch inference and/or cap parallelism (ORT is multi-threaded on its own).
- **Peak RSS per model: ≈3.1 GB (bge-m3-int8), ≈2.4 GB (MiniLM-L12, float weights), ≈0.8 GB (bge-small)** against the laptop budget of 16 GB — compatible, but the inference phase must not overlap in time with heavy ANN operations; the RAM budget and the disk-backed/quantized index requirement come from the "Hard constraints" in openspec/config.yaml.
- **Stabilization of ort.** Runs on rc.13; before the production `embedding` crate, repeat the resolution check (a new rc may be out) — the seam mechanism does not depend on the version.
