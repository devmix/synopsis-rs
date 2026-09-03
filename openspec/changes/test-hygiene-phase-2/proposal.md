# Proposal: test-hygiene-phase-2

## Problem

`test-hygiene-phase-1` (archived 2026-09-02) shrank the 12 worst-offender
source files by relocating their inline `#[cfg(test)]` test modules into
integration test files. It was deliberately scoped to those 12 files. The
broader goal — shrink the remaining large application-code files (the user's
"some files are up to 2,000 lines") — is not complete. A fresh audit of the
current tree shows the files ≥ 1,200 lines that still carry a large inline
test module and were **not** handled by phase 1:

| File | Total lines | Inline test-module lines |
|---|---:|---:|
| `crates/config/src/preset.rs` | 2,427 | ~1,114 |
| `crates/mcp/src/server.rs` | 1,414 | ~657 |
| `crates/cli/src/serve/server.rs` | 1,430 | ~650 |
| `crates/db/src/entity.rs` | 1,369 | ~883 |
| `crates/config/src/ontology.rs` | 1,320 | ~318 |
| `crates/mcp/src/tools/dossier.rs` | 1,287 | ~645 |
| `crates/vectors/src/usearch/mod.rs` | 1,258 | ~408 |

(`crates/cli/src/serve/watcher.rs`, 1,263 / ~729, is excluded: phase 1
confirmed all 15 of its tests touch private internals and stay inline.)

## Solution

One extraction task per file (7 files), following the phase-1 pattern exactly:
relocate the file's movable tests into `crates/<crate>/tests/<name>.rs`, leave
the stay-inline tests (those that need private internals) in place, and add a
`#[doc(hidden)] pub mod test_support` seam where the moved tests need a few
`pub(crate)` items. A final verification task reconciles before/after line and
test counts and runs the full gates.

## Frozen contracts touched

**None.** No MCP tool, CLI surface, data schema, or config format changes.
Tests are relocated, not rewritten or added; production logic is untouched
apart from the narrow `test_support` visibility seams (docs-hidden, test-only,
not part of any frozen contract). No new dependencies.

## Non-goals

- `crates/cli/src/serve/watcher.rs` stays inline (all tests use private
  internals — phase-1 decision, unchanged).
- Files under 1,200 lines (the ~100 smaller inline-test files) are out of
  scope for this change; they can be a follow-up `test-hygiene-phase-3` if the
  user wants to go further.
- No new test coverage; no test logic is added or modified — pure relocation.
- No production-logic refactor; only visibility widening (`pub(crate)` →
  exposed via `test_support`) to feed the integration tests.
- No changes to `parity-harness`, the fixture format, or the Go oracle
  (`../synopsis` remains untouched — this is Rust-side hygiene, not a port).
- No new production or dev dependencies (a `cel`-style re-use of an existing
  dependency is allowed only if it is already a production dependency of the
  crate at the same version, as phase-1 did for `graph`).
