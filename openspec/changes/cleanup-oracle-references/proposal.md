# Proposal: cleanup-oracle-references

## Why

The Go oracle at `../synopsis` is a read-only reference implementation. During
the migration, every Rust module carried a `../synopsis/...` doc-comment tag
mapping it back to its Go origin (e.g. `//! Oracle mapping:
\`../synopsis/internal/mcp/handlers/pagination.go\``). The migration is **complete**
and the parity harness that consumed those mappings is **removed** (change
`remove-parity-harness`). The tags are now migration-provenance noise in
production code, and several now point at a deleted mechanism (the parity
harness) — making them stale, not just noisy.

The user decided (2026-09-03): **remove `../synopsis/...` path references from
production crate code, keep them in `openspec/specs/` and the archived changes**
(where they are legitimate contract/historical provenance).

## What

Remove all 101 `../synopsis/...` path references from production crate source
(`crates/*/src/**`, 96 refs) and crate `Cargo.toml` comments (5 refs), **preserving
meaningful behavioral / design / wire text** and dropping pure provenance tags.
No spec, contract, code-logic, or dependency changes. `openspec/specs/**`,
`openspec/changes/archive/**`, test-fixture provenance
(`crates/config/tests/data/README.md`, `fixtures/README.md`), migration files,
and ADRs (`docs/adr/**`) keep their references. `../synopsis` is not read or
modified.

## Impact

- Affected specs: **none** (doc-comment only; the `mcp-contract`, `cli-surface`,
  and other specs already carry the authoritative provenance and are untouched).
- Affected crates: `cli`, `config`, `db`, `embedding`, `graph`, `ingestion`,
  `llm`, `mcp`, `search` (all 9 crates that carry refs; `vectors` and `utils`
  have none).
- Risk: **low.** All 101 refs are `//!`/`///` doc comments or `#` Cargo.toml
  comments — no code, no runtime path, no `include_str!`. `cargo fmt/clippy/test`
  stay green; the only invariant is `rg '\.\./synopsis' crates/*/src/
  crates/*/Cargo.toml` → 0.

## Non-goals

- Do NOT touch `openspec/specs/**` (contract provenance stays).
- Do NOT touch `openspec/changes/archive/**` (history stays).
- Do NOT touch test-fixture provenance docs, migration SQL, or ADRs.
- Do NOT modify `../synopsis`.
- Do NOT change any code logic, public API, or dependency.
