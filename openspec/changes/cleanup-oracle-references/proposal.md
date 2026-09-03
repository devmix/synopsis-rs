# Proposal: cleanup-oracle-references

## Why

The Go project at `../synopsis` **will be deleted**. It was a read-only
reference implementation during the migration; its behavior, tests, and
contracts were the source of truth. Now that the migration is complete and the
parity harness is removed (change `remove-parity-harness`), the Rust project is
a standalone product. The user decided (2026-09-03): the Rust documentation
should no longer reference the Go project or frame the code as a port of it —
it should describe a **native Rust codebase**.

So this change removes all **migration-provenance** — both the `../synopsis/...`
paths *and* the surrounding narrative ("the oracle", "Go original", "ported",
"re-architected, not transcribed", "deviations from the oracle", the AGENTS.md
`## Oracle` / `## Migration principles` sections) — from every living doc, while
**preserving the design rationale** (why the code behaves as it does), reframed
as native Rust decisions.

## What

- **Crates** (`crates/*/src/**`, 9 crates; `Cargo.toml` comments): remove all
  oracle/Go/ported narrative from doc comments, keeping the design rationale.
- **`AGENTS.md`**: remove the `## Oracle` and `## Migration principles` sections;
  reword the intro, frozen-stack, gotchas, and Layout table to describe a
  standalone Rust MCP server.
- **`README.md`**: remove the "Migration status" oracle framing and the
  "parity was machine-checked during the migration" paragraph.
- **`openspec/config.yaml`, `docs/adr/**`, `openspec/specs/**`** (Russian):
  remove the oracle/Go/ported narrative in Russian (the English translation is a
  separate change, `translate-to-english`).

**`openspec/changes/archive/**` is NOT touched** — it is the historical audit
trail and must not be rewritten.

## Impact

- Affected specs: the main contract specs under `openspec/specs/**` are edited
  to drop their Go-provenance framing, but their **contract content is unchanged**
  (requirements, tool schemas, CLI surface, data schema stay identical).
- Affected crates: `cli`, `config`, `db`, `embedding`, `graph`, `ingestion`,
  `llm`, `mcp`, `search` (all with doc-comment refs; `vectors` and `utils` have
  none).
- Risk: **low for crates** (comment-only; `cargo fmt/clippy/test` stay green),
  **moderate for the top-level docs** (AGENTS.md section removals and README
  rewording change prose, not behavior). No code, contract, or dependency
  changes. The only invariants: no oracle/Go/ported narrative remains in living
  docs, and the archive is untouched.

## Non-goals

- Do NOT touch `openspec/changes/archive/**` (history stays, even though it is
  full of oracle references).
- Do NOT translate the Russian docs (that is `translate-to-english`).
- Do NOT modify `../synopsis` (it will be deleted; not our concern).
- Do NOT change any code logic, public API, contract content, or dependency.
- Do NOT remove the project's own design-decision references (`D1…D8`,
  `ADR 0001…0005`) or wire-format versions (`mcp-go v0.57.0`).
