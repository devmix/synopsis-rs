# Design: remove the unused `DocumentChunk.doc_id` field

## D1 — Delete the field outright (no deprecation path)

**Problem.** `DocumentChunk.doc_id: Option<i64>` is never assigned (always
`None`) and never read in production; it documents a design ("series change
3") that was never implemented.

**Decision.** Remove the field, its doc comment, and every
`doc_id: None` construction-site initializer; delete the tests that only
assert the field is `None`.

**Alternatives considered.**

- Keep it as a placeholder for a future write-stage assignment — rejected:
  speculative API surface; YAGNI, and the misleading doc comment is an
  active liability.
- Deprecate (`#[deprecated]`) then remove — rejected: single-crate internal
  type, no external consumers, one workspace; a deprecation cycle buys
  nothing.

**Reference specs/fixtures.** None — no contract spec or recorded fixture
mentions `DocumentChunk` (verified by grep over `openspec/specs/` and the
parity fixtures); the gate is the workspace test suite.

## D2 — Leave the DB-layer `Chunk.doc_id` untouched

**Problem.** The grep for `doc_id` also matches the DB row struct
`Chunk` (`crates/db/src/chunk.rs`) whose `doc_id` is the real FK to
`documents` (used by `list_by_doc_id`, `count_by_doc_id`, FTS joins, GC).

**Decision.** Scope the change to `DocumentChunk` only; the DB struct, its
DAO, and every local `doc_id: i64` variable in the write stage / runner /
e2e tests stay as-is.

**Alternatives considered.** Renaming `Chunk.doc_id` for symmetry —
rejected: it is used, spec-pinned (data-schema), and out of scope.
