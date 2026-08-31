# Proposal: post-migration-lance-removal

## Problem

The lance/usearch engine comparison period is over: usearch won (ADR 0004; archived
changes `add-usearch-ann-engine` and `usearch-wal-persistence`), and the user decided
(2026-08-31) to remove LanceDB and everything related to it completely. Lance still
exists as: the `LanceEngine` (~1000 lines) + `engine-lance` Cargo feature, the `lancedb`
dependency tree (pulls datafusion/arrow — a large dependency graph), the A/B-benchmark
transitional code in `parity-harness`, engine-feature plumbing in `cli`, the
`vectors.engine = "lance"` config value, IVF-only config fields (`num_partitions`,
`nprobes`) that do not apply to usearch HNSW, the CI `ci/darwin-sdk` workaround (exists
only because `lance-arrow` links `-framework CoreFoundation`), and references in
AGENTS.md, README.md, `openspec/config.yaml`, the main specs, module docs, and
`.github/workflows/ci.yml`.

## Solution

1. **vectors crate:** delete `LanceEngine` and all `engine-*` Cargo features — usearch
   becomes the single, unconditional engine. Remove the `lancedb` and orphaned `futures`
   dependencies, the `engine-lance`-gated test targets (`integration_gates`,
   `full_benchmark`), and the IVF-only `num_partitions`/`nprobes` config fields.
   Factory: `""`/`"usearch"` → `UsearchEngine`; `"lance"` → explicit
   "engine removed" error; unknown → error.
2. **cli + workspace:** remove the `engine-lance`/`engine-usearch` feature plumbing;
   revert the `vectors = { default-features = false }` experiment (features no longer
   exist); fix the `unwrap_or("lance")` log fallback in bootstrap; drop lance fixtures
   from cli tests.
3. **parity-harness:** remove the transitional A/B-benchmark code
   (`tests/ab_benchmark_test.rs`, `src/bench.rs` if orphaned) and engine-feature
   plumbing. The harness itself (fixtures, mcp_client, metrics, diff) stays — it is
   permanent dev tooling.
4. **config crate:** `vectors.engine` accepts only `"usearch"` (or absent); `"lance"`
   → explicit validation error naming the removal.
5. **Docs/specs/CI:** update `openspec/config.yaml` frozen stack, main specs (deltas
   below), ADR 0003 gets a minimal "Superseded by ADR 0004" status line (ADR body
   untouched — immutable history), AGENTS.md, README.md, module doc comments,
   `.github/workflows/ci.yml` (drop the darwin-sdk workaround), delete `ci/darwin-sdk/`.

## Frozen contracts touched

- **Config format (user-approved 2026-08-31):** `vectors.engine` value `"lance"` is
  removed (explicit error instead of a working engine); `num_partitions`/`nprobes`
  removed from the index config struct (never exposed in presets — internal only).
  Existing presets set no `vectors:` section, so no shipped preset breaks.
- **MCP tools:** untouched.
- **CLI surface:** no behavior change; `db clear` still wipes the dataset state dir
  (wording in the spec updated: "Lance-индекс" → vector index).
- **Data schema:** untouched. `usearch_vectors_log` unchanged; the
  `<vectors_path>/usearch` engine-tagged subdirectory layout is unchanged, so existing
  dataset data keeps working.
- **vector-index spec:** engine-selection requirement replaced with a single-engine
  requirement; production-gates and configuration requirements reworded from
  IvfHnswSq/IVF internals to usearch HNSW (gate numbers unchanged — they are
  human-accepted contracts).

## Non-goals

- Changing usearch engine behavior or persistence (ADR 0004 stands).
- Rebuilding or moving existing dataset vector data (path layout unchanged).
- Translating the main specs to English (Phase 2, `post-migration-oracle-cleanup`).
- Rewriting ADR 0003 (immutable history; supersede status line only).
- Any MCP transport change (the legacy SSE task is a separate change).
- Touching `openspec/changes/archive/`, `.archive/spikes/`, `docs/adr/spike-s3-results.md`
  (historical records).
