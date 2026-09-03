# Proposal: remove-parity-harness

## Problem

The Go→Rust migration is **complete**: all 38 module changes are archived and
synced to the main specs. The `crates/parity-harness` crate was a **transitional
dev-tooling** crate whose only job was to machine-check parity during the
migration — an rmcp MCP-client wrapper with p50/p95 timing, a fixture loader
(the `SYNX`/`vectors.bin` dump + recorded tool-response JSON), and JSON/text diff
utilities. Now that the port is done, the crate is dead weight:

- **Zero inbound dependencies.** No product crate, test, or CI job references
  `parity-harness` as a dependency (verified: the only references are the
  workspace-member entry and doc/`Cargo.toml`-comment mentions).
- **Outside the product dependency graph** (design D1) — it was never part of
  the shipped binary.
- It is referenced only in **living docs** that now mis-describe the workspace:
  `AGENTS.md` (crate table, dependency graph, commands table, a stale
  `vectors.bin` gotcha), `README.md` (Done/In-progress status, commands table,
  Parity section), `crates/mcp/Cargo.toml` + `tests/server_integration.rs`
  (comments), `COVERAGE.md`, and `docs/port-verification-report.md`.

## Solution

Delete `crates/parity-harness/` (20 files, including the fixtures) and remove
the workspace member from the root `Cargo.toml`. Then remove every **non-archived**
reference to the crate from the living docs above. While touching the README,
update its **Migration status** from "In progress" to **Complete** (the port is
done). Archived changes under `openspec/changes/archive/` are **not** touched —
they are historical provenance.

## Frozen contracts touched

**None.** No MCP tool, CLI surface, data schema, or config format changes. No
production code path, public API, or dependency changes — the crate is dev
tooling outside the product graph, and the edits elsewhere are doc/comment-only.
No new or removed workspace dependencies beyond the dropped member (the
`Cargo.lock` `[[package]] parity-harness` entry is pruned automatically).

## Non-goals

- `vectors::synx` (the `SYNX`/`vectors.bin` binary-format module in `crates/vectors`)
  is **kept**: it is public product code and the format contract fixed by
  `native-seam-spikes` D4. `parity-harness` was its only *consumer*, but it is
  `pub` API, not dead code, and removing it is a separate decision out of scope here.
- The Go oracle (`../synopsis`) is **not** touched — read-only reference.
- `openspec/changes/archive/**` is **not** touched — historical provenance is
  preserved as-is (user decision 2026-09-03, Q1).
- No new production or dev dependencies; no dependency re-pinning.
- The recorded parity fixtures (`crates/parity-harness/fixtures/**`) are removed
  with the crate (transitional evidence; the port is complete and the parity
  results are preserved in the archived change results + `port-verification-report.md`).
