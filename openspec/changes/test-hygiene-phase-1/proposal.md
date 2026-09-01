# Proposal: test-hygiene-phase-1

## Problem

Application-code source files are bloated by inline `#[cfg(test)] mod tests` blocks. A
workspace audit (see design.md, Appendix A) measured 122 inline test modules across 160
files (1,460 tests). The worst offenders are majority-test files:

| File | Total lines | Test lines | Test % |
|---|---|---|---|
| `crates/llm/src/client.rs` | 1,401 | 1,181 | 84% |
| `crates/db/src/fact.rs` | 1,823 | 1,275 | 70% |
| `crates/ingestion/src/ingester/mod.rs` | 1,526 | 1,076 | 70% |
| `crates/graph/src/linker.rs` | 1,809 | ~900 | ~53% |
| `crates/graph/src/cel.rs` | 1,740 | ~900 | ~52% |

The goal (user Phase 1) is to shrink the application-code files by relocating test code
into integration test files (`crates/<crate>/tests/`), remove duplicate tests, and remove
tests that exercise code never used outside tests.

Audit findings that shape this change:
- **Duplicates:** 0 true duplicate `#[test]` functions. Exactly **one** true duplicate —
  the `test_platform_key` helper, byte-identical in `cli/src/onnx_runtime.rs:212` and
  `embedding/src/lib.rs:326`.
- **Dead test-only code:** **none** to delete. Every "test-only" item is deliberate
  `#[cfg(test)]` test infrastructure (seams/fixtures); the tests that use them exercise
  real production behavior. (Confirmed no-op — user decision C.)
- **Extraction:** of 256 inline tests across the 12 flagged files, 173 are movable with
  the existing public API, 79 are unlockable via a narrow `#[doc(hidden)] pub mod
  test_support`, and 33 must stay inline (private internals).

## Solution

1. **Dedup (task 1.1):** eliminate the single `test_platform_key` duplicate by making
   both helpers delegate to the production `current_platform_key()` (the pattern
   `embedding/src/library.rs:411` already uses).
2. **Per-file extraction (tasks 1.2–1.12):** one task per source file, ordered. Each task
   relocates the file's movable tests into `crates/<crate>/tests/<name>.rs`, leaving the
   stay-inline tests (private internals) in place. Where a file's tests need a few private
   items, the task adds a `#[doc(hidden)] pub mod test_support` (always-compiled,
   docs-hidden) exposing exactly those items (user decision A).
3. **Verification (task 1.13):** full workspace gates + a before/after line-count and
   test-count reconciliation.

## Frozen contracts touched

**None.** No MCP tools, CLI surface, data schema, or config format is changed. Two
deliberate, non-contract surface notes:
- `#[doc(hidden)] pub mod test_support` is added to 4 crates (`llm`, `mcp`, `ingestion`,
  and re-exported items). It is docs-hidden, test-only, and not part of any frozen
  contract.
- `cel` is added to `graph` `[dev-dependencies]` for the extracted `cel.rs` tests. `cel`
  is already a `graph` production dependency at the same version — **no new package, no
  Cargo.lock change**.

Parity is unaffected: the parity-harness fixtures and the differential/ANN gates are not
touched; the full suite must stay green (1,460 tests) with the same assertions.

## Non-goals

- No new test coverage is added; tests are relocated or deduplicated, not written.
- No production logic is refactored — only visibility widening (`pub(crate)`) to feed
  `test_support`, and the one dedup delegation.
- `crates/cli/src/serve/watcher.rs` is **not** extracted: all 15 of its tests touch
  private internals (`Debouncer`, `Watcher.task`, `debounce_loop`, …) and stay inline.
- The 33 stay-inline tests across other files are not forced to move by widening the
  public API beyond the narrow `test_support` items.
- No changes to `parity-harness` fixtures, the `vectors.bin` format, or the Go oracle.
- No new production dependencies.
