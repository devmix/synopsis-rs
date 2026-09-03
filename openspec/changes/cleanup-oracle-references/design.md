# Design: cleanup-oracle-references

## Context note

All 101 references are `//!`/`///` doc comments (96) or `#` Cargo.toml comments
(5). There is **no code-level** `../synopsis` usage (verified: no `Path::new`,
no `include_str!`, no string literal). So this is pure comment editing — code
logic, public API, and dependencies are untouched, and `cargo fmt/clippy/test`
stay green by construction.

## The rule (uniform across all tasks)

For each `../synopsis/...` reference:

1. **Pure provenance tag** — a doc-comment line whose only purpose is the
   oracle mapping (e.g. `//! Oracle mapping: \`../synopsis/...\``,
   `//! Oracle: \`../synopsis/...\``, `//! Oracle reference: \`../synopsis/...\``)
   → **delete the whole line** (and drop any continuation line that only
   elaborates the mapping and becomes dangling).
2. **Meaningful text + path** — a line that carries real behavioral/design
   content plus a path (e.g. `//! Wire reference: mcp-go v0.57.0 (pinned in
   \`../synopsis/go.mod\`)`, `//! Deliberate deviation from the oracle
   (\`../synopsis/internal/llm/client.go\`)`) → **remove the path, keep the
   meaningful text** (reword so the sentence still reads cleanly).
3. **Stale parity-harness clause** — a clause that cites the (now-removed)
   parity harness as an acceptance criterion (e.g. `rrf.rs`: "differential
   parity with `../synopsis/internal/search/rrf_test.go` is an acceptance
   criterion") → **drop the clause** (the mechanism no longer exists; keeping
   the path would leave a dangling reference).
4. **Cargo.toml** — a `# D1 edges; oracle imports: ../synopsis/...` comment →
   **keep the design note (`# D1 edges`), drop the `oracle imports: ...`
   clause**. A `# Binary name matches the Go oracle artifact
   (../synopsis/bin/synopsis).` → keep the note, drop the path.

## Decisions

### D1 — Preserve meaningful text; the goal is path removal, not doc deletion

The point is to strip the migration-provenance *path*, not to delete useful
documentation. Deviation notes ("Deliberate deviation from the oracle"), wire
contracts ("mcp-go v0.57.0"), config-resolution rules, and binary-naming notes
all stay; only the `../synopsis/...` path (and pure mapping tags) go. The
reviewer checks that no meaningful text is lost.

### D2 — Drop now-stale parity-harness clauses

Change `remove-parity-harness` deleted the differential-harness crate. Any
doc line asserting "differential parity with `../synopsis/...` is an acceptance
criterion" is now false. Those clauses are removed together with the path.

### D3 — Keep provenance where it belongs

`openspec/specs/**` (authoritative contract provenance), `openspec/changes/
archive/**` (history), test-fixture READMEs, migration SQL, and ADRs are
**out of scope** and keep their references. Only production crate source and
crate `Cargo.toml` are cleaned.

## Oracle references

None. `../synopsis` is not read or modified; the references being removed are
the only connection, and their provenance value is already captured in
`openspec/specs/` and the archive.
