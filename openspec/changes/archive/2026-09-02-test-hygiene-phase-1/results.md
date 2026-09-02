# Test Hygiene Phase 1 — Results

Machine-verified final report for change `test-hygiene-phase-1` (tasks 1.1–1.12). All
measurements taken against the committed tree (post-task-1.12). No source files were
modified by the verification task (1.13) itself.

## 1. File shrinkage (11 extracted files, tasks 1.2–1.12)

"Before" = line count at the extraction commit's parent (`git show <commit>^:<file>`);
"After" = current line count.

| File | Before | After | Δ lines | Reduction |
|---|---:|---:|---:|---:|
| `crates/db/src/fact.rs` | 1,823 | 547 | −1,276 | 70.0% |
| `crates/cli/src/serve/bootstrap.rs` | 1,403 | 598 | −805 | 57.4% |
| `crates/mcp/src/tools/documents.rs` | 1,139 | 496 | −643 | 56.5% |
| `crates/mcp/src/tools/graph_tools.rs` | 1,181 | 460 | −721 | 61.1% |
| `crates/graph/src/cel.rs` | 1,740 | 843 | −897 | 51.6% |
| `crates/search/src/hybrid.rs` | 1,142 | 640 | −502 | 44.0% |
| `crates/graph/src/linker.rs` | 1,809 | 1,469 | −340 | 18.8% |
| `crates/llm/src/client.rs` | 1,401 | 642 | −759 | 54.2% |
| `crates/mcp/src/transport/sse.rs` | 1,312 | 934 | −378 | 28.8% |
| `crates/ingestion/src/ingester/mod.rs` | 1,526 | 449 | −1,077 | 70.6% |
| `crates/ingestion/src/runner/mod.rs` | 1,454 | 917 | −537 | 36.9% |
| **Total (11 files)** | **15,930** | **7,995** | **−7,935** | **49.8%** |

`crates/cli/src/serve/watcher.rs` (1,263 lines) — **not extracted**: all 15 of its tests
use private items (`Debouncer`, `debounce_loop`, `relevant_kind`, `wanted_extension`/
`normalize_extensions`, `watchable_sources`, `IngestChangeHandler::handle_changes`,
`Watcher.task`) and are out of scope (design Appendix A).

## 2. Test-count reconciliation

Workspace total: **1,474 (before, Appendix A baseline) → 1,474 (after). Zero delta** —
relocations only.

Per crate (before → after, via `cargo test -p <crate> -- --list`):

| Crate | Before | After | Δ |
|---|---:|---:|---:|
| cli | 134 | 134 | 0 |
| config | 120 | 120 | 0 |
| db | 187 | 187 | 0 |
| embedding | 101 | 101 | 0 |
| graph | 95 | 95 | 0 |
| ingestion | 377 | 377 | 0 |
| llm | 42 | 42 | 0 |
| mcp | 183 | 183 | 0 |
| parity-harness | 42 | 42 | 0 |
| search | 102 | 102 | 0 |
| utils | 8 | 8 | 0 |
| vectors | 83 | 83 | 0 |
| **Total** | **1,474** | **1,474** | **0** |

Run breakdown: **1,469 passed + 5 ignored = 1,474; 0 failed.** The 5 `#[ignore]`d tests
are pre-existing (counted in the baseline 1,474 `--list` total) and untouched by this
change (pure relocation, no test-logic edits).

True-duplicate removals per design D4: **0**. All 222 movable/needs-pub tests were MOVED
to integration files, not removed; no name+body duplicates were found against the
pre-existing integration tests.

## 3. Residual inline test inventory

Files that kept a `#[cfg(test)]` block — **34 stay-inline tests** total (matches
Appendix A's STAY_INLINE total):

| File | Tests | Private-item reason |
|---|---:|---|
| `crates/search/src/hybrid.rs` | 3 | `invert_score_reciprocal` → private `invert_score` (:361); `standalone_results_maps_hits` → private `standalone_results` (:336); `hybrid_fusion_pool_is_max_of_leg_tops` → private `HybridSearcher::fusion_pool` (:203) |
| `crates/graph/src/linker.rs` | 9 | `cross_domain_pairs` ×2 → private fn; `llm_*` ×7 → private `MockLlm` (:1283) |
| `crates/llm/src/client.rs` | 2 | `new_accepts_valid_config`, `new_accepts_zero_max_retries` → read private `LlmClient.config` |
| `crates/mcp/src/transport/sse.rs` | 5 | `message_without_session_id`, `message_with_unknown_session_id`, `message_with_malformed_body`, `message_round_trip`, `touched_sse_stream` → need `#[cfg(test)] test_server()` (`handle_message` takes `State<SseState{sessions, server}>`) |
| `crates/cli/src/serve/watcher.rs` | 15 | all private — not extractable, out of scope |
| `crates/ingestion/src/runner/mod.rs` | 0 test fns | retains the 7 shared fixtures (`TestSource`/`MockEmbedding`/`MemoryIndex`/`Harness`/`TempDir`/`source_config`/`TEMP_COUNTER`) in a reduced `#[cfg(test)]` block — imported by the 6 out-of-scope `cleanup.rs` tests (cleanup.rs:216); Option A (task 1.12 Revision 1) |
| **Total** | **34** | |

> Counting note: `sse.rs:843` and `sse_units.rs:291` each contain a **comment** with the
> literal `#[tokio::test(start_paused = true)]` text. Attribute-grep counts must exclude
> comment lines — sse.rs has 5 inline tests (not 6) and sse_units.rs has 18 (not 19).

## 4. New integration test files created

| File | Tests | From |
|---|---:|---|
| `crates/db/tests/fact.rs` | 30 | `db/src/fact.rs` |
| `crates/cli/tests/serve_bootstrap.rs` | 22 | `cli/src/serve/bootstrap.rs` |
| `crates/mcp/tests/documents.rs` | 19 | `mcp/src/tools/documents.rs` |
| `crates/mcp/tests/graph_tools.rs` | 17 | `mcp/src/tools/graph_tools.rs` |
| `crates/graph/tests/cel.rs` | 29 | `graph/src/cel.rs` |
| `crates/search/tests/hybrid_units.rs` | 12 | `search/src/hybrid.rs` |
| `crates/graph/tests/linker_units.rs` | 7 | `graph/src/linker.rs` |
| `crates/llm/tests/client.rs` | 32 | `llm/src/client.rs` |
| `crates/mcp/tests/sse_units.rs` | 18 | `mcp/src/transport/sse.rs` |
| `crates/ingestion/tests/ingester.rs` | 23 | `ingestion/src/ingester/mod.rs` |
| `crates/ingestion/tests/runner.rs` | 13 | `ingestion/src/runner/mod.rs` |
| **Total moved** | **222** | |

Reconciliation: 222 moved + 34 stay-inline = 256 (the audited 12 files' test count).

## 5. test_support seams created

Three `#[doc(hidden)] pub mod test_support;` seams were added to expose `pub(crate)`
helpers to the new integration files (the integration tests link against the non-test lib
build, so `pub(crate)` items are unreachable):

- `crates/llm/src/test_support.rs` — thin `pub fn` wrappers over `pub(crate)` methods
  (E0432: `pub use` cannot re-export inherent methods).
- `crates/mcp/src/test_support.rs` — thin `pub fn` delegates + `pub const X =
  crate::…::X;` references (E0364: `pub use` cannot re-export a `pub(crate)` item).
- `crates/ingestion/src/test_support.rs` — `walk_matched_files` delegate (reused by both
  `tests/ingester.rs` and `tests/runner.rs`).

## 6. Gate outputs (task 1.13)

| Gate | Command | Result |
|---|---|---|
| fmt | `cargo fmt --all --check` | PASS (clean) |
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | PASS (no warnings) |
| check | `cargo check --workspace` | PASS |
| test | `cargo test --workspace` | PASS (1,469 passed + 5 ignored = 1,474; 0 failed) |

## 7. Design decisions recorded during implementation

- **Task 1.7 (hybrid) Revision 1:** `fusion_pool` is a private method; the one test that
  needs it stays inline (E0624 — cannot be reached from an integration test).
- **Task 1.12 (runner) Revision 1 — Option A (human decision 2026-09-01):** Appendix A
  classified all 13 runner tests as movable, but the shared fixtures are also imported by
  the 6 out-of-scope `cleanup.rs` tests. Option A keeps the fixtures inline (reduced
  `#[cfg(test)]` block) and gives the moved tests local copies (D4 convention);
  `cleanup.rs` is untouched and the task's file scope is respected. Acceptance criterion
  #1 ("no `#[cfg(test)]` block") is superseded — runner/mod.rs keeps the fixtures.
- **E0364 / E0432 patterns** (tasks 1.9–1.11): `pub use` cannot re-export `pub(crate)`
  items or inherent methods for integration tests — resolved with thin `pub fn` delegates
  and `pub const` compile-time references.
- **`../synopsis` path-free doc convention** (task 1.4 Revision 1): new integration test
  files carry no relative `../synopsis/…` path in their module doc comments.

No production logic was changed by any task in this phase; `../synopsis` was untouched
throughout.
