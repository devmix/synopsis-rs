# Proposal: remove the unused `DocumentChunk.doc_id` field

## What

Remove the dead `doc_id: Option<i64>` field from `DocumentChunk`
(`crates/ingestion/src/types.rs:94`) and update every construction site and
test reference.

## Why

The field is a leftover of a design that was never implemented (the doc
comment says "`None` until the write stage (series change 3) assigns it"):

- Every construction site — production chunkers
  (`chunkers/{mediawiki,markdown,json}.rs`) and all test fixtures — sets
  `doc_id: None`.
- No production reader exists: the write stage
  (`ingester/mod.rs`) uses a local `doc_id` variable (`docs.create(...)` /
  `existing.id`) and never reads `chunk.doc_id`.
- The only references are tests asserting the field is `None`
  (`types.rs`, one assertion per chunker test module).
- `DocumentChunk` derives only `Debug, Clone, Default, PartialEq` — it is
  not a wire format, so nothing serializes the field.

Keeping it misleads readers into thinking the write stage assigns it.

## Scope

- `crates/ingestion/src/types.rs` — the field + its doc comment + the
  test-module references.
- `crates/ingestion/src/chunkers/{mediawiki,markdown,json}.rs` — one
  construction site + one test assertion each.
- `crates/ingestion/tests/{ingester,runner}.rs`,
  `crates/ingestion/src/{runner/mod,worker}.rs` (test modules) — test
  fixtures.

## Frozen contracts touched

**None.** `DocumentChunk` is internal to `crates/ingestion` (re-exported for
the crate's own pipeline, not part of the MCP tools, CLI surface, data
schema, or config formats); no spec under `openspec/specs/` mentions it. The
DB-layer `Chunk.doc_id` (`crates/db/src/chunk.rs`, the FK to `documents`) is
a different struct and is untouched. Parity: no recorded fixture references
the field (it is never serialized).

## Behavior change (explicit)

None observable: the field is always `None` and never read. Public API of
the `ingestion` crate loses one always-`None` field.

## Non-goals

- Do NOT touch `Chunk.doc_id` in `crates/db/src/chunk.rs` (used FK).
- Do NOT touch local `doc_id: i64` variables in `ingester/mod.rs`,
  `runner/mod.rs` (`clear_and_delete_doc`), or `tests/pipeline_e2e.rs` —
  DB-layer plumbing.
- Do NOT change any other `DocumentChunk` field, derive, or the chunkers'
  behavior.
- No spec deltas (no contract spec mentions `DocumentChunk`).
