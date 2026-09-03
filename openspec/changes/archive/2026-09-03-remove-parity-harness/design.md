# Design: remove-parity-harness

## Context note

Pure removal of a **transitional dev-tooling crate** plus doc/comment cleanup.
No behavior, contract, public-API, or dependency change. The Go oracle
(`../synopsis`) is **not** the reference for this change (nothing is being
ported) and **must remain untouched**. This is post-migration housekeeping.

## Verification (ground truth)

- `crates/parity-harness/` = **20 files** (Cargo.toml, `src/*.rs`, `tests/*.rs`,
  `examples/record_content.rs`, `fixtures/**` including `vectors.bin`).
- **Inbound dependency check:** no `[dependencies]`/`[dev-dependencies]` entry in
  any other crate names `parity-harness`; the only references are (a) the
  workspace-member line in root `Cargo.toml`, and (b) doc/`Cargo.toml`-comment
  mentions. `crates/mcp/Cargo.toml` lines 11/40 and
  `crates/mcp/tests/server_integration.rs` lines 5/161 are **comments only** —
  the mcp test inlines the pattern and does not import the crate.
- `.github/workflows/ci.yml` has **no** `parity-harness` references.
- `vectors::synx` is referenced only by `crates/vectors/src/lib.rs`
  (`pub mod synx;` + its doc comment); `parity-harness` was its sole consumer.

## Decisions

### D1 — Delete the whole crate directory, fixtures included

Remove `crates/parity-harness/` in full, not just the code. The fixtures
(`fixtures/content/*.json`, `fixtures/vectors.bin`) are the one-time-recorded
oracle outputs that the harness compared against. They are transitional parity
evidence; the port is complete, and the parity *results* are already captured in
the archived change results and `docs/port-verification-report.md`. Keeping the
raw fixture bytes with no consumer would be dead data.

*Why not move the fixtures elsewhere:* the user decision (2026-09-03) was to
**remove parity-harness entirely**; relocating the fixtures would contradict the
"transitional tool is gone" intent and has no consumer to serve.

### D2 — Keep `vectors::synx` (do not remove the format module)

`crates/vectors/src/synx.rs` implements the `SYNX` binary fixture format
(`native-seam-spikes` D4). `parity-harness` was its only consumer, but it is
`pub` API of a product crate — removing it is a **separate** decision (and would
be a public-API change to `vectors`). It compiles cleanly with no consumer and
triggers no `dead_code` warning (public items are reachable). Out of scope here.

### D3 — `Cargo.lock` update is expected and in-scope

Removing the workspace member causes the next `cargo` invocation to prune the
`[[package]] name = "parity-harness"` entry (and any package that was pulled in
*only* by it — expected to be none, since its deps are shared with other crates).
The updated `Cargo.lock` is part of the change. This is not a "new dependency" —
it is the inverse of one.

### D4 — Doc references removed from living docs only; archive untouched

References in `AGENTS.md`, `README.md`, `crates/mcp/Cargo.toml`,
`crates/mcp/tests/server_integration.rs`, `COVERAGE.md`, and
`docs/port-verification-report.md` are removed/rewritten. References under
`openspec/changes/archive/**` are **left as-is** (historical provenance — user
decision 2026-09-03, Q1). The stale `vectors.bin` gotcha in `AGENTS.md` is
dropped outright: the harness it pointed at is gone and `native-seam-spikes` has
landed (the format now lives in `vectors::synx`).

### D5 — README migration status → Complete (folded in)

The README's "Migration status" currently says "In progress: module-by-module
porting". Since the port is complete, this is updated to **Complete** in the same
task that removes the README's parity-harness references (it already touches that
file, so no separate micro-change is needed — user decision 2026-09-03, Q5).

## Oracle references

None. Nothing is ported in this change; `../synopsis` is not read or modified.
