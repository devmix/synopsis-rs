# Results: test-hygiene-phase-2 — final verification (task 2.8)

Read-only verification, 2026-09-03. Baseline commit: `5bab57a`
("docs(openspec): add test-hygiene-phase-2 change plan" — the pre-extraction
baseline). No source code was modified during this verification. `../synopsis`
untouched.

## 1. Per-file before/after line counts

Before = `git show 5bab57a:<path> | wc -l`; after = `wc -l <path>`.

| File | Before | After | Reduction |
|---|---:|---:|---:|
| `crates/config/src/preset.rs` | 2427 | 1311 | 1116 |
| `crates/db/src/entity.rs` | 1369 | 484 | 885 |
| `crates/mcp/src/server.rs` | 1414 | 755 | 659 |
| `crates/cli/src/serve/server.rs` | 1430 | 780 | 650 |
| `crates/mcp/src/tools/dossier.rs` | 1287 | 640 | 647 |
| `crates/vectors/src/usearch/mod.rs` | 1258 | 974 | 284 |
| `crates/config/src/ontology.rs` | 1320 | 998 | 322 |
| **Total** | **10505** | **5942** | **4563** |

All 7 before-counts match the task-body numbers exactly (2,427 / 1,369 / 1,414 /
1,430 / 1,287 / 1,258 / 1,320). Reductions match the task-body inline-test-line
estimates (~1,114 / ~883 / ~657 / ~650 / ~645 / ~318) within a few lines. The
`vectors/src/usearch/mod.rs` reduction (284) is smaller than its ~408 estimate
by design: only the `flush_tests` module (before-file lines 961–1258) was moved
to `vectors/tests/usearch_units.rs`; the `test_util` fixture module and the 5
sibling inline test modules stayed (see §4).

All 7 integration test files from design D5 exist:
`config/tests/preset.rs`, `db/tests/entity.rs`, `mcp/tests/server_units.rs`,
`cli/tests/serve_server.rs`, `mcp/tests/dossier.rs`,
`vectors/tests/usearch_units.rs`, `config/tests/ontology.rs`.

## 2. Per-crate test-count reconciliation

Current (after) counts via `cargo test -p <crate> -- --list`; before = the
pre-change baselines recorded at `5bab57a`.

| Crate | Baseline (before) | Current (after) | Delta |
|---|---:|---:|---:|
| config | 120 | 120 | 0 |
| db | 192 | 192 | 0 |
| mcp | 183 | 183 | 0 |
| cli | 135 | 135 | 0 |
| vectors | 83 | 83 | 0 |

All five crates match their baselines exactly — zero delta. `config` was
touched by both tasks 2.1 and 2.7 and `mcp` by both tasks 2.3 and 2.5; the net
still equals the baseline in both cases (relocation only, no test added or
removed).

## 3. Gate outputs

| Gate | Command | Result |
|---|---|---|
| Format | `cargo fmt --all --check` | PASS (exit 0, no diff) |
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` | PASS (exit 0, no warnings) |
| Tests | `cargo test --workspace` | PASS (exit 0) |

`cargo test --workspace`: 59 test binaries, all ok; **1510 tests total:
1505 passed, 0 failed, 5 ignored**.

## 4. Residual inline-test inventory

Verified against the actual files (`grep -n '#\[cfg(test)\]'`):

| File | `#[cfg(test)]` block retained? | Reason |
|---|---|---|
| `config/src/preset.rs` | No — fully removed | All tests movable; relocated to `config/tests/preset.rs` |
| `db/src/entity.rs` | No — fully removed | All tests movable; relocated to `db/tests/entity.rs` |
| `mcp/src/server.rs` | No — fully removed | All tests movable; relocated to `mcp/tests/server_units.rs` |
| `cli/src/serve/server.rs` | No — fully removed | All tests movable; relocated to `cli/tests/serve_server.rs` |
| `mcp/src/tools/dossier.rs` | No — fully removed | All tests movable; relocated to `mcp/tests/dossier.rs` |
| `vectors/src/usearch/mod.rs` | **Yes** — `mod test_util` retained (line 865) | Shared fixture module (`TempDir` + config / vector / WAL-table / DISK-segment helpers) referenced by exactly 5 sibling inline test modules: `keys_manifest.rs:146`, `layout.rs:234`, `search.rs:199`, `wal.rs:335`, `compaction.rs:317` (all via `use super::super::test_util::…`). Moving `test_util` out would require a visibility widening beyond the narrow `test_support` seam — out of scope for this change. |
| `config/src/ontology.rs` | No — fully removed | All tests movable; relocated to `config/tests/ontology.rs` |

The 5 sibling inline test modules named in the task (keys_manifest, layout,
search, wal, compaction) live in their own submodule files, each with its own
`#[cfg(test)]` block: `keys_manifest.rs:139`, `layout.rs:218`, `search.rs:187`,
`wal.rs:320`, `compaction.rs:297`. They stay inline because they consume the
shared `test_util` fixtures and private internals of their modules.

Note: the task body phrases these as "retained in `vectors/usearch/mod.rs`";
in the actual tree only `test_util` is in `mod.rs` — the 5 test modules are in
their respective sibling files (the `usearch` module tree). Wording difference
only; the intent (test_util + 5 modules stay inline, flush_tests moved) is
fully preserved.

Out of scope for this change: `vectors/src/lib.rs:385` and
`vectors/src/synx.rs:200` also carry `#[cfg(test)] mod tests` blocks; both
pre-date phase 2 (at `5bab57a`: `lib.rs:380`, `synx.rs:200`) and those files
were not among the 7 extraction targets.

## 5. Known minor residual (recorded, not fixed)

`crates/vectors/tests/persistence_integration.rs` line 11 contains a stale
prose reference to `usearch::flush_tests` in the module doc comment ("crash
matrix: … `usearch::flush_tests` (missing sidecar / segment) …"). The module
was moved to `crates/vectors/tests/usearch_units.rs`, whose header documents
the verbatim move from `flush_tests`. This is plain `//!` prose, not a rustdoc
intra-doc link, so it does not break rustdoc. Recorded as a known minor
residual and a candidate for a future cleanup; deliberately NOT fixed per task
2.8 (report, don't silently fix).

## 6. Discrepancies found

1. **Wording only (non-blocking):** task 2.8 §4 attributes the 5 inline test
   modules (keys_manifest, layout, search, wal, compaction) to
   `vectors/usearch/mod.rs`; they actually live in their own sibling files,
   with only `test_util` in `mod.rs`. Behavior/intent match the task exactly.
2. **None substantive:** all three gates pass; every crate's test count is
   unchanged from baseline; all 7 files shrank by ~the relocated test-module
   lines.

Working-tree note (unrelated to source): `git status` shows two modified files
at verification time — `.idea/synopsis-rs.iml` (IDE metadata) and
`openspec/changes/test-hygiene-phase-2/tasks.md` (task checkboxes being ticked
off as tasks complete — expected workflow artifact).

## Verdict

Task 2.8 acceptance met: all three gates pass, every crate's test count is
unchanged (zero delta), all 7 files shrank as expected, and this `results.md`
is written.
