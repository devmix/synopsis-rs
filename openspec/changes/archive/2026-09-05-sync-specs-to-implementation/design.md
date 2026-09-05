# Design — sync-specs-to-implementation

## Context

See `proposal.md` (Why). The nine drifts were surfaced by the docs migration
(`2026-09-05-migrate-docs-site-to-rust`), which documented the *actual* Rust implementation.
The implementation is correct and shipped; the specs are stale. This change corrects the specs
(and one AGENTS.md line) to match. Constraint: each edit must be a **no-breaking-change**
correction — it aligns spec text to behavior the test suite already pins.

Reference contract specs and fixtures for this design:
- Main specs being corrected: `openspec/specs/{config-format,cli-surface,data-schema,db-storage,knowledge-graph,vector-index}/spec.md`.
- Recorded fixtures / sources of truth: the effective-config machine-diff tests (config-format),
  the `--help` + load-test report fixtures (cli-surface — the report fixture already records
  `seed: 42`), the init migrations (`migrations/knowledge/1-init/up.sql`,
  `migrations/cache/1-init/up.sql`), and the `crates/config` parsing tests (vestigial fields).

## Goals / Non-Goals

**Goals:** make the six spec deltas + the AGENTS.md line + the NER template re-sync exactly
match the shipped implementation; keep every edit a spec-to-reality alignment (no behavior
change).

**Non-Goals:** behavior changes, new features, wire/CLI/data-contract changes (see proposal).

## Decisions

### D0. This is a sanctioned frozen-contract correction (separate explicit decision)

AGENTS.md states that changing a frozen contract (MCP tools / CLI / data schema / config) is a
**separate explicit decision** with justification; it never happens inside an implementation
task. The archived docs change (`2026-09-05-migrate-docs-site-to-rust`, tasks.md "Deferred
follow-ups (spec corrections)") explicitly deferred these nine items to a dedicated
spec-correction change and named the mechanism (`opsx-propose` → edit the spec deltas →
`opsx-sync`). This change IS that dedicated change: it carries only the corrections, each with
a justification, and touches no unrelated contract. That is the "separate explicit decision."

### D1. config-format legacy keys — REMOVE, do not mark "accepted-but-ignored"

**Decision:** drop the four legacy keys (`database.path`, `paths.data_dir`,
`paths.documents_dir`, `paths.global_config_path`) from the `Full YAML preset` key list
entirely; the real keys are `paths.workspace_dir` (plus `migrations_dir`, `prompts_path`,
`onnx_config`) and the knowledge-DB path is *derived* from `paths.workspace_dir` +
`dataset.name`.

**Why:** the keys were removed/renamed by the archived `storage-layout-restructure` change,
which carried **no spec delta** — so the spec was never synced and still lists keys the parser
no longer reads. The config struct (`crates/config/src/preset.rs`) has no such fields:
`PathsConfig` is `{ workspace_dir, migrations_dir, prompts_path, onnx_config }` and
`DatabaseConfig` is `{ pragma }` only (the knowledge-DB path is intentionally not a field —
`DatasetConfig::db_path` derives it). Listing them as "accepted-but-ignored" would assert a
tolerance the code does not special-case and would re-introduce the exact ambiguity the
restructure removed (two spellings of the same path).

**Rejected alternative — mark-as-ignored:** keep the keys in the spec as "accepted but
ignored". Rejected because (a) the code does not read them at all, so the spec would describe
behavior that does not exist; (b) it would contradict the restructure's intent that the
knowledge-DB path has exactly one spelling (derived), and (c) it keeps four dead keys alive in
the contract forever. Removing them makes the spec match the shipped parser exactly.

### D2. NER template — re-sync the EMBEDDED default to the SHIPPED config

**Decision:** make the embedded default `crates/ingestion/src/ner/templates/user.tmpl`
byte-identical to the shipped `workspace/configs/prompts/ner/user.tmpl` (the fenced
`CONTENT SECTION` format). This is the **one code change** in this change and requires updating
3 test assertions in `crates/ingestion/src/ner/prompts.rs` (~lines 512, 515–517, 530, 539) and
4 doc comments (`prompts.rs:24/171/264`, `llm.rs:11`).

**Why:** AGENTS.md line 70 and `workspace/configs/README.md` line 19 both claim the shipped
`workspace/configs/prompts/**` are **byte-identical** to the embedded defaults, and that claim
is load-bearing — the template-source SHA-256 hashes are part of the NER decision-cache key, so
byte-identity is what makes the cache key identical whether the file or the embedded default is
used. Today the entity-linker pair is byte-identical but the NER pair is not: the shipped file
uses a fenced `CONTENT SECTION` block while the embedded default uses a `Document context:`
block. Re-syncing the embedded default to the shipped file restores the claim **without
touching either the claim or the shipped config** — the shipped config is the source of truth
(the user-facing, tracked file), so the embedded default is the side that moves.

**Rejected alternative — amend the claim instead of re-syncing:** edit AGENTS.md line 70 +
`workspace/configs/README.md` line 19 to drop the byte-identity claim (or carve out NER).
Rejected because (a) it weakens a real, useful invariant (cache-key stability) that is true for
the entity-linker pair and should be true for NER too; (b) it leaves two divergent templates for
the same prompt, so a user who deletes the override silently gets a different prompt than the
one the docs show; and (c) the drift note itself lists "re-sync the template or amend the
claim" and re-syncing is the lower-entropy fix (one file + its tests, no doc edits).

### D3. No-breaking-change guarantee

Every edit aligns spec text to **shipped, test-pinned** behavior; none changes what the binary
does:

- config-format / cli-surface / vector-index: the corrected key/flag lists are exactly what the
  existing effective-config, `--help`, and config-parsing tests already assert (the load-test
  report fixture already records `seed: 42`).
- data-schema / db-storage / knowledge-graph: the corrected table placement is exactly what the
  init migrations create (`app_kv` + `llm_linker_cache` in `migrations/cache/1-init/up.sql`) and
  what the DAO/queue tests already exercise.
- The only code change (D2) brings the embedded default to match the already-shipped config; the
  3 updated test assertions assert the new (correct) rendered format, and `cargo test -p
  ingestion` must stay green. No CLI flag, config key, DB table, or MCP tool changes behavior.

## Risks / Trade-offs

- **D1 removal could look like a "breaking" config change** — it is not: the keys were already
  gone from the parser; the spec is merely catching up. A preset that still carries a legacy key
  parses today (unknown keys do not break startup) and will continue to; the spec now simply
  stops advertising keys that no longer exist.
- **D2 re-sync changes the embedded default's rendered prompt** — the shipped config already
  renders that way, so the effective prompt for any user with the override (the tracked default
  path) is unchanged; only users relying on the *embedded* fallback (no override file) see the
  fenced format, which is the intended, documented format.
- **Spec-only edits are low-risk but not zero** — a MODIFIED requirement replaces the old one on
  sync, so each delta must carry the full corrected requirement text with all still-valid
  scenarios preserved (this is enforced by reading each current spec before writing its delta).
