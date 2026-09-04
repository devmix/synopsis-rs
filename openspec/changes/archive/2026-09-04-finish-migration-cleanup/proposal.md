# Proposal: finish-migration-cleanup

## Why

Change `cleanup-oracle-references` (archived 2026-09-04) removed the
migration-provenance narrative (the Go project at `../synopsis`, "the oracle",
"ported", `../synopsis/...` paths) from the crates, `AGENTS.md`, `README.md`,
`openspec/config.yaml`, `docs/adr/**`, `openspec/specs/**`, and
`.opencode/agents/**`. Its `3.1` verification was scoped to exactly those paths.
A comprehensive sweep of the **rest** of the repo found the cleanup is not yet
complete: ~49 stale references remain in 5 files that were outside that scope,
plus a migration-era artifact (`fixtures/`, the v5 Go knowledge-base copy) and a
one-time port-verification report that still frame the project as a port of a Go
original. The Go project will be deleted, so the repo must be fully autonomous.

## What

- **Remove the v5 parity fixture** (`fixtures/`): a gitignored copy of the Go
  `knowledge.db` that can no longer be regenerated once `../synopsis` is gone,
  ships in no checkout (gitignored), and whose only consumers (`db::test_util`
  helpers + `crates/db/tests/fts5_parity.rs`) skip cleanly when absent. FTS5
  behavior is verified independently elsewhere.
- **Reframe stale oracle/Go comments** in 4 files:
  `migrations/knowledge/1-init/up.sql`, the workspace `Cargo.toml`,
  `.github/workflows/ci.yml`, and the `.gitignore` Cargo.lock comment.
- **Delete `docs/port-verification-report.md`** — a one-time migration
  milestone report, redundant with the archived OpenSpec changes + ADRs (the
  canonical record of the deliberate decisions).

**`openspec/changes/archive/**` is NOT touched** (historical audit trail).

## Impact

- Affected specs: none (this is a cleanup; no contract content changes).
- Affected crates: `db` only (removal of the fixture test helpers + one
  integration-test file). No public API, contract, or dependency changes.
- Risk: **low**. Task 1.1 removes test-only helpers and one test file (gates
  stay green; the 4 fixture tests are the only ones removed). Tasks 1.2 and 1.3
  are comment-only / file-deletion (no behavior, SQL, CI-logic, or YAML change).
- Invariants: the corrected acceptance pattern returns **0** across the whole
  repo (excluding `node_modules` and the untouched archive), and
  `cargo fmt/clippy/test` stay green.

## Non-goals

- Do NOT touch `openspec/changes/archive/**` (history stays).
- Do NOT touch `.opencode/node_modules/**` (third-party).
- Do NOT translate the Russian docs (that is the separate `translate-to-english`
  change).
- Do NOT change any code logic, public API, contract content, dependency, or the
  SQL/CI/YAML behavior of the reframed files.
- Do NOT remove the project's own design-decision references (`D1…D8`,
  `ADR 0001…0005`) or the legitimate **DB-migration** concept
  (`migrations`, `PRAGMA user_version`, "the v5 schema shape").
