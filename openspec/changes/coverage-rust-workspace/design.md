## Context

- The workspace has 1,474 tests (Phase 1 test-hygiene complete) but **no coverage
  measurement**. We need a per-crate coverage baseline to target gaps for the remaining
  phases.
- **Frozen stack:** no new crates in `Cargo.lock`. A coverage tool is dev-only and must not
  pollute the product `Cargo.lock` or the binary.
- **CI:** a single `.github/workflows/ci.yml` with `checks` and `cross-builds` jobs. CI
  already installs `cargo-zigbuild` via `cargo install --locked` — the precedent for a
  cargo-installed dev tool.
- **Toolchain:** pinned to Rust 1.96.0 (`rust-toolchain.toml`); CI uses
  `dtolnay/rust-toolchain@<rev>` which must match that file exactly.
- **Measured baseline (planning smoke test, `cargo llvm-cov --workspace`):** overall line
  coverage is already **~93%** (42,352 lines, 2,983 missed). Per-crate: db 97.98%, graph
  97.44%, llm 97.78%, utils 97.79%, ingestion 96.11%, search 96.43%, mcp 95.64%, vectors
  94.96%, config 94.62%, embedding 88.09%, cli 76.90%. **Every crate is already above its
  D4 target.** The residual gaps concentrate in the integration-heavy / FFI crates (cli
  ~1,391 missed lines, embedding ~308) — expected, since serve/sync dispatch and ONNX FFI
  error paths are hard to unit-test.

## Goals / Non-Goals

**Goals:**
- A reproducible per-crate **line-coverage** baseline via a dev-only tool.
- A CI job that produces an `lcov.info` artifact (parallel to `checks`).
- Documented, realistic per-crate coverage targets for a 16 GB-laptop personal tool.
- A one-command local workflow (`cargo llvm-cov --workspace --html`).

**Non-Goals:**
- No coverage gates (`--fail-under-*`) — measure-first; gating is Phase 3.
- No external coverage service (Codecov, etc.).
- No changes to product/test source, the frozen stack, or `Cargo.lock`.

## Decisions

### D1 — Tool: `cargo-llvm-cov` v0.9.0 (cargo-installed binary)

**Choice:** `cargo-llvm-cov` v0.9.0, installed via `cargo install cargo-llvm-cov`
(CI: `taiki-e/install-action@cargo-llvm-cov`).

**Why over alternatives:**
- **vs `cargo-tarpaulin`:** tarpaulin uses dynamic instrumentation — slower (~3–5×), no
  branch coverage, less precise, and less actively maintained. llvm-cov uses `rustc
  -C instrument-coverage` (the native LLVM mechanism), is faster, and supports line +
  branch coverage.
- **vs `cargo-nextest` built-in:** nextest is a test runner, not a coverage tool; it
  integrates *with* llvm-cov but does not replace it.
- **Version/MSRV (verified online):** v0.9.0 (released 2026-08-16), 7.5M+ downloads, no
  known CVEs. Supports Rust 1.82–1.98 (LLVM 19–22); the pinned 1.96.0 uses LLVM 19 — in
  range. Prebuilt binaries exist for the CI targets.

### D2 — Mechanism: cargo-installed binary (NOT a dev-dependency)

**Choice:** install the binary into `CARGO_HOME`; do **not** add it to any `Cargo.toml`.

**Why:** the frozen-stack rule governs `Cargo.lock` entries. A cargo-installed binary adds
nothing to `Cargo.lock` and nothing to the product binary — identical to the existing
`cargo-zigbuild` precedent. A dev-dependency would (a) enter `Cargo.lock`, (b) risk pulling
transitive deps into the product graph, and (c) violate the frozen-stack rule. This is the
cleanest, rule-compliant mechanism.

### D3 — CI integration: new job in `ci.yml`, parallel to `checks`

**Choice:** add a `coverage` job to `.github/workflows/ci.yml`:
1. `dtolnay/rust-toolchain` (matching `rust-toolchain.toml`) + `llvm-tools-preview`
   component.
2. `taiki-e/install-action@cargo-llvm-cov`.
3. `cargo llvm-cov --workspace --lcov --output-path lcov.info`.
4. `actions/upload-artifact` for `lcov.info` (and `html` if cheap).

**Why:** keeps coverage out of the fast `checks` job (coverage instrumentation slows the
build); a separate job means a coverage failure never blocks the core gate in Phase 2
(measure-first). No external service upload (Non-goal).

### D4 — Strategy: measure-first, per-crate targets documented (not gated)

**Choice:** Phase 2 produces the baseline report + documented targets only. Gating
(`--fail-under-lines` per crate) is deferred to Phase 3.

**Per-crate targets (documented in a `COVERAGE.md` at repo root):**

| Crate | Target | Rationale |
|---|---|---|
| `db` | 70–80% | core data layer, DAO/FTS5 logic |
| `search` | 70–80% | hybrid search, RRF fusion, enrichment |
| `vectors` | 70–80% | ANN engine, usearch FFI, compaction |
| `ingestion` | 60–70% | parsing, chunking, NER pipelines |
| `mcp` | 60–70% | tool handlers, transport, dispatch |
| `config` | 60–70% | presets, ontology, NER config |
| `graph` | 60–70% | knowledge graph, CEL linkers |
| `utils` | 60–70% | shared helpers |
| `llm` | 60–70% | LLM client, retry/backoff |
| `cli` | 50–60% | subcommand dispatch, flags (integration-heavy) |
| `embedding` | 50–60% | ONNX runtime lifecycle (complex FFI) |
| `parity-harness` | excluded | test infrastructure, not product code |

**Why measure-first:** setting hard gates before we know the real baselines risks
immediate breakage from over-aggressive targets. Establish the numbers, then gate in
Phase 3 with confidence.

**Baseline finding (informs Phase 3):** the smoke-test measurement shows every crate is
already **above** its target (overall ~93%). So the value of this change is (a)
institutionalizing the measurement so coverage cannot silently regress in later phases,
and (b) identifying the specific uncovered lines (the ~7%, concentrated in `cli`/`embedding`)
for targeted follow-up. Because the baselines are high, Phase 3 gating is low-risk — and
the targets may be **raised** (e.g. cli/embedding toward 80%) once the gap list is in hand.

### D5 — Baseline report location

**Choice:** commit a machine-readable baseline snapshot
(`openspec/changes/coverage-rust-workspace/baseline.md`) with the per-crate line-coverage
numbers at the time of this change, plus a `COVERAGE.md` at repo root describing the
command + targets. The full HTML/lcov stays a CI/local artifact (not committed — large).

## Risks / Trade-offs

- **[LLVM version mismatch between rustc and llvm-cov]** → mitigated: Rust 1.96.0 = LLVM
  19, within llvm-cov v0.9.0's supported range (19–22); the CI install-action downloads
  prebuilt binaries matching the toolchain.
- **[Coverage job slows CI]** → mitigated: separate job, parallel to `checks`; does not
  block the core gate.
- **[Flaky / non-reproducible coverage numbers]** → low: llvm-cov is deterministic for a
  given build + test set; the baseline snapshot is recorded once.
- **[Frozen-stack dispute over the tool]** → mitigated: cargo-installed (like
  `cargo-zigbuild`), zero `Cargo.lock` impact; documented in D2.
- **[Targets too ambitious for a personal tool]** → mitigated: targets are ranges, not
  gates, in Phase 2; they inform Phase 3 gating, not the build.

## Migration Plan

1. Add the `coverage` CI job (does not affect existing jobs).
2. Run the baseline locally + in CI; record `baseline.md`.
3. Commit `COVERAGE.md` + `baseline.md`.
- **Rollback:** remove the CI job + the two docs; the tool is cargo-installed, so nothing
  in `Cargo.lock`/source changes. Zero product impact.

## Open Questions

- None — all material decisions (tool, mechanism, strategy, targets, CI) are resolved and
  user-approved (2026-09-02).
