## Why

The 11 workspace crates use bare names (`config`, `db`, `utils`, `search`, `graph`, …)
that collide with popular crates.io package names, making the codebase ambiguous to
readers, tooling, and future agents (e.g. `use db::…` does not say which `db`). Giving
every internal crate the `synopsis-` prefix makes the project ownership of each crate
explicit and eliminates the name-collision ambiguity. Pure rename — no logic, no
behavior change.

## What Changes

- All 11 workspace crates are renamed to the `synopsis-` prefix:
  `cli → synopsis-cli`, `config → synopsis-config`, `db → synopsis-db`,
  `embedding → synopsis-embedding`, `graph → synopsis-graph`,
  `ingestion → synopsis-ingestion`, `llm → synopsis-llm`, `mcp → synopsis-mcp`,
  `search → synopsis-search`, `utils → synopsis-utils`, `vectors → synopsis-vectors`.
- Crate directories move `crates/<name>` → `crates/synopsis-<name>` via `git mv`
  (history preserved).
- `package.name` in each crate `Cargo.toml`, the workspace `members` list, and the
  `[workspace.dependencies]` palette keys are updated.
- Rust crate identifiers in source are renamed `X::` → `synopsis_X::` across all
  crates (~910 path references in ~110 `.rs` files); mechanical, no logic change.
- `Cargo.lock` churn: the 11 internal packages and their dependency references are
  renamed; no external package is added, removed, or re-versioned.
- Documentation updated in the same change (AGENTS.md layout table, README.md,
  site/docs, docs/adr, `openspec/specs/utils/spec.md`, `workspace/configs/README.md`,
  `.opencode/agents/*.md`). Archived changes under `openspec/changes/archive/**` are
  NOT modified (historical record).

**Non-breaking**: the binary name stays `synopsis` (`[[bin]] name` untouched), the CLI
surface, MCP wire contract, data schema, and config formats are all unchanged.

## Capabilities

### New Capabilities

(none)

### Modified Capabilities

(none — pure rename refactor, no spec-level behavior change; `skip_specs: true`)

## Impact

- Code: all 11 crates under `crates/` (Cargo.toml package names, directory renames,
  identifier renames in `src/` and `tests/`), root `Cargo.toml`, `Cargo.lock`.
- Docs: AGENTS.md, README.md, site/docs (~30 lines), docs/adr (2 files),
  `openspec/specs/utils/spec.md`, `workspace/configs/README.md`,
  `.opencode/agents/` (4 files).
- CI: no change — `.github/workflows/ci.yml` and `release.yml` contain no crate
  paths (verified); `scripts/release.sh` contains no crate paths either.
- No new dependencies; frozen stack untouched.

## Frozen contracts

None touched. The frozen contracts are the MCP tools (wire contract), the CLI surface
(binary `synopsis`, subcommands, flags), the data schema (`migrations/`,
`PRAGMA user_version`), and the config formats (`workspace/configs`, `onnx.yaml`).
Crate names are internal to the build and invisible on the wire. Parity is confirmed
by the machine: the full `cargo test` suite (including the MCP differential tests
against recorded fixtures — fixtures unchanged) plus a byte-identical
`synopsis --help` before/after the change.

## Non-goals

- No changes to crate contents, public API shape, module layout, or logic.
- No changes to the binary name, CLI surface, MCP tools, data schema, or config formats.
- No changes to external dependency versions or to the frozen stack.
- No rewriting of archived OpenSpec artifacts (`openspec/changes/archive/**`).
- No changes to CI workflows, release scripts, or the build system.
- No per-crate `package = "synopsis-*"` aliasing of the old short names — the rename
  is complete: code uses `synopsis_X::` identifiers.
