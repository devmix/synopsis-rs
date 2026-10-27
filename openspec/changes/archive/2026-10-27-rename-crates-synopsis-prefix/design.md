# Design — rename-crates-synopsis-prefix

## Context

The workspace has 11 crates with bare names under `crates/` (see proposal.md — Why).
Each crate is referenced three ways: `package.name` in its `Cargo.toml`, the
workspace `members` list + `[workspace.dependencies]` palette key in the root
`Cargo.toml`, and Rust identifiers (`use db::…`, `config::…`) in the source of every
dependent crate — ~910 path references across ~110 `.rs` files. Internal crates are
wired via workspace inheritance (`X = { workspace = true }`), so a rename touches the
palette key, each dependent's `[dependencies]`/`[dev-dependencies]` entry, and all
identifier use-sites. Gates after every change: `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, `cargo test` (green, ~20 s warm).

## Goals / Non-Goals

**Goals:**

- Every internal crate is named `synopsis-*` in `package.name`, directory, palette
  key, and Rust identifier (`synopsis_*`) — one naming scheme everywhere.
- The build is green (fmt/clippy/test) after every subtask; no intermediate red
  state.
- Docs and agent definitions updated in the same change (AGENTS.md rule: docs are
  part of the change).
- Machine-confirmed parity: full test suite incl. MCP differential fixtures
  unchanged; `synopsis --help` byte-identical before/after.

**Non-Goals:**

- No API, logic, module-layout, or behavior changes (see proposal.md — Non-goals).
- No aliasing of old short names; after the change the bare identifiers
  (`db::`, `config::`, …) must not occur in `.rs` files.

## Decisions

### D1 — Full rename (package + identifiers), not package-only aliasing

User decision (2026-10-27, option a). `package.name` becomes `synopsis-db` and code
uses `synopsis_db::…`.

- Alternative: keep bare identifiers via `db = { package = "synopsis-db", path =
  "crates/synopsis-db" }` aliases in `[workspace.dependencies]`. Rejected: it leaves
  exactly the ambiguous identifiers this change exists to remove, and maintains two
  naming schemes.
- Consequence: ~910 identifier lines change; each line is edited exactly once, in the
  subtask of the crate it references, so total diff is order-independent.

### D2 — Directories renamed with `git mv`

`crates/<name>` → `crates/synopsis-<name>` in the same subtask as the package rename
(user approved). `git mv` preserves file history; the alternative (keep bare
directory names) would leave directory/package mismatch.

### D3 — One subtask per crate, in dependency order

Order: `utils → llm → config → db → vectors → embedding → ingestion → graph →
search → mcp → cli`, then a docs subtask, then final verification.

- Each subtask renames exactly one crate (directory + package + palette key) and
  updates all references to it in not-yet-renamed dependents (`[dependencies]`,
  `[dev-dependencies]`, `X::` identifiers in `src/`/`tests/`).
- Dependency order (base tier first) keeps the per-subtask diff smallest where the
  reference counts are small and keeps the build green throughout: a subtask never
  references a crate that does not yet exist under its new name.
- Alternative considered: rename one crate's own files only and do a final
  "update all references" pass — rejected: the build would be red between subtasks.
- Alternative: a single mega-task — rejected by the ≤500-line diff rule (total
  ~2000 diff lines).

### D4 — The `db` subtask exceeds the ~500-line diff limit (user-approved)

`db` has 427 identifier references (~850 diff lines) and the rename is atomic —
splitting it (e.g. via a temporary `package =` alias) would either leave a red build
or a dual naming state. User explicitly approved the exception (2026-10-27, answer 4):
one commit, ~850 diff lines, purely mechanical.

### D5 — Identifier replacement pattern requires the `::` boundary

Mechanical replacement uses a word-boundary + `::`-suffix pattern
(`\bdb::` → `synopsis_db::`, etc.) so prose ("the db crate"), error strings, and
config keys are never touched. No crate name is a substring of another internal name,
and no external dependency shares an internal name, so the patterns cannot hit
foreign identifiers. After replacement, `cargo fmt` + `clippy` + `test` are mandatory
in the same subtask.

### D6 — Baseline capture for parity

Before the first subtask, record `synopsis --help` (and the `--help` of each
subcommand) output to a temp file outside the repo. The final verification subtask
diffs the post-rename output against it. MCP parity needs no new fixtures: the
existing differential tests run unchanged.

### D7 — Docs updated in the same change; archive untouched

AGENTS.md (layout table, dependency-graph line, gotchas), README.md, site/docs,
docs/adr (2 files), `openspec/specs/utils/spec.md`, `workspace/configs/README.md`,
and `.opencode/agents/*.md` (4 files) are updated in a dedicated subtask after all
crate renames. `openspec/changes/archive/**` is a historical record and is NOT
modified. CI workflows and `scripts/release.sh` contain no crate paths (verified) —
no change needed.

## Risks / Trade-offs

- [Over-broad mechanical replacement hits prose/strings] → Pattern requires the
  `::` boundary (D5); gates + `rg '\b(db|config|…)::' -g '*.rs'` empty-check after
  each subtask.
- [`db` subtask ~850 diff lines > 500-line rule] → User-approved exception (D4);
  reviewer verifies the diff is purely mechanical (1:1 line replacements, no logic).
- [Cargo.lock churn obscures review] → Expected mechanical churn of 11 internal
  packages; reviewer confirms no external package version changed.
- [Missed reference in a non-`.rs` file (e.g. a test fixture path, build script)] →
  Final subtask runs a repo-wide `rg` for `crates/(cli|config|…)\b` outside
  `openspec/changes/archive/**` and `Cargo.lock`; gates catch anything functional.
- [AGENTS.md edited twice (release commit + docs subtask)] → Separate commits; no
  conflict expected (different sections).

## Migration Plan

1. Capture `synopsis --help` baseline (D6).
2. Execute subtasks 1.x in order (D3); each ends with green fmt/clippy/test and its
   own commit.
3. Docs subtask (D7) — own commit.
4. Final verification subtask: gates + `rg` empty-checks + `synopsis --help`
   byte-diff + `cargo build --release` produces `target/release/synopsis`.
5. Rollback: each subtask is one commit — `git revert` per commit; directory moves
   came from `git mv`, so history and revert are clean. Full abort: revert all
   subtask commits.

## Open Questions

(none — all ambiguities resolved by the user on 2026-10-27: full rename, directory
renames, docs in the same change, `db` limit exception, staged release work
committed separately first).
