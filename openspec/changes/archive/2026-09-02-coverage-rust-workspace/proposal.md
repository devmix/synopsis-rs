## Why

After Phase 1 (test-hygiene) the workspace has strong test **volume** (1,474 tests) but no
visibility into **coverage** — which code paths are actually exercised. The remaining
migration phases (performance, docs) need a coverage baseline to target real gaps
(untested error paths, edge cases, FFI/ONNX boundaries) instead of guessing. No coverage
tooling exists yet, and it must be added without violating the frozen-stack rule
(no new crates in `Cargo.lock`).

## What Changes

- Add **`cargo-llvm-cov` v0.9.0** as a cargo-installed dev tool — the same mechanism CI
  already uses for `cargo-zigbuild`. Zero impact on `Cargo.lock`, the product binary, or
  non-coverage build times. (Verified: compatible with the pinned Rust 1.96.0 / LLVM 19,
  7.5M+ downloads, no known CVEs.)
- Add a **coverage job** to `.github/workflows/ci.yml` (parallel to `checks` and
  `cross-builds`): install `llvm-tools-preview` + `cargo-llvm-cov`, run
  `cargo llvm-cov --workspace --lcov --output-path lcov.info`.
- Generate and commit a **baseline per-crate coverage report** (line coverage).
- **Document per-crate coverage targets** for a 16 GB-laptop personal tool (not a
  library/server): 70–80% core (`db`/`search`/`vectors`), 60–70% (`ingestion`/`mcp`/
  `config`/`graph`/`utils`/`llm`), 50–60% integration-heavy (`cli`/`embedding`);
  `parity-harness` excluded (test infrastructure, not product code).
- **Measure-first:** this change establishes the baseline report only — **no coverage
  gates** (`--fail-under-*`). Gating is deferred to Phase 3 once targets are validated.

## Capabilities

### New Capabilities
None.

### Modified Capabilities
None.

This is a **tooling** change: it adds development coverage measurement and does not change
any product behavior or frozen contract, so it sets `skip_specs: true` (no spec deltas).

## Impact

- **Frozen contracts: NONE affected.** MCP tools, CLI surface, data schema, and config
  formats are unchanged. Parity: N/A — no behavior changes, so the existing machine parity
  gates (recall@k / p50/p95) are untouched.
- **CI:** one new job in `.github/workflows/ci.yml`; no change to the product build or the
  cross-build matrix.
- **New dev-only tool:** `cargo-llvm-cov` (cargo-installed binary, not a `Cargo.toml`
  dependency — does not enter `Cargo.lock`).
- **No source-code changes** to any `crates/**` product or test file.

## Non-goals

- **No coverage gates** in this change (measure-first; `--fail-under-*` gating deferred to
  Phase 3).
- **No external coverage service** (Codecov, etc.) — local + CI artifacts only; this is a
  personal tool, not a public library.
- **No changes** to test code, product code, the frozen stack, or `Cargo.lock`.
- **No parity fixture expansion** — that is the separate change `parity-fixture-expansion`.
- **No branch-coverage gating** — line coverage only for the baseline.
