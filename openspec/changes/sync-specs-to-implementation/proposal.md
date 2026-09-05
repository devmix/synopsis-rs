# Proposal — sync-specs-to-implementation

## Why

The docs migration (archived change `2026-09-05-migrate-docs-site-to-rust`) rewrote the
documentation site against the *actual* Rust implementation. Doing so surfaced **nine
spec-vs-implementation drifts** (recorded in that change's `tasks.md` → "Deferred follow-ups
(spec corrections)"). In every case the **spec text (and one AGENTS.md line) is the stale side**:
the Rust implementation is already correct, tested, and shipped. This change corrects the specs
(and that one AGENTS.md line) so they match the shipped behavior. It is a sanctioned
frozen-contract correction: the archived change explicitly deferred these to a dedicated
spec-correction change, and AGENTS.md requires any frozen-contract change to be a separate,
explicit decision — which is exactly what this change is.

## What Changes

Ten corrections. Six are spec deltas (MODIFIED requirements, full corrected text — the sync
replaces the old requirement); the remainder are short text edits outside `openspec/specs/`
(AGENTS.md and the `openspec/config.yaml` frozen-stack context):

1. **config-format** — `Full YAML preset`: remove the four legacy keys
   (`database.path`, `paths.data_dir`, `paths.documents_dir`, `paths.global_config_path`);
   the real keys are `paths.workspace_dir` + the derived knowledge-DB path
   (`<workspace_dir>/datasets/<name>/state/knowledge.db`).
2. **config-format** — `onnx.yaml model registry`: download verification is
   **size-enforced; checksum optional/unverified** (not "URL + size + SHA-256").
3. **cli-surface** — `Global flags and command-line structure`: drop the nonexistent
   `--db PATH` global; the real globals are `--config`, `--preset`, `--dataset`, `--version`.
4. **cli-surface** — `load-test subcommand`: add the missing `--seed N` (default 42).
5. **data-schema** — `Compatibility with the v5 schema`: `app_kv` is NOT a knowledge-DB table
   (it lives in the cache DB, `migrations/cache/1-init/up.sql`).
6. **db-storage** — `DAO operations over the v5 schema`: `app_kv` is NOT a knowledge-DB DAO
   target (it lives in the cache DB).
7. **knowledge-graph** — `Cross-domain linking pipeline`: the LLM decision cache is the
   `llm_linker_cache` table (cache DB), not `app_kv`.
8. **AGENTS.md line 17** — the frozen-stack line says ONNX provides "embeddings **+ NER**";
   correct to **embeddings only** (NER is `RegexNer` rule-based + `LlmNer` via `crates/llm`).
9. **vector-index** — `Index configuration` + `Insertion and kNN search`: `num_partitions`/`nprobes`
    **persist as vestigial/unused config fields** (present for tolerant parsing, unused by
    `UsearchEngine`), not "removed with the lance engine" (the stale wording appeared in two
    requirements; both corrected).
10. **openspec/config.yaml** — the frozen-stack context (shown to AI agents) repeats two of the
    drifts: line 16 says ONNX provides "embeddings … **and NER**" (correct to **embeddings only**,
    matching the AGENTS.md line 17 fix) and line 17 says the onnx.yaml registry is "URL +
    size_bytes + **sha256-verify**" (correct to "size enforced; checksum optional", matching the
    config-format onnx.yaml fix).

Plus **one code change** (the only code change in this change): **NER template re-sync** — make
the embedded default `crates/ingestion/src/ner/templates/user.tmpl` byte-identical to the
shipped `workspace/configs/prompts/ner/user.tmpl` (the fenced CONTENT SECTION format), restoring
the byte-identity claim that AGENTS.md line 70 and `workspace/configs/README.md` line 19 already
make. This requires updating 3 test assertions in `crates/ingestion/src/ner/prompts.rs`
(~lines 512, 515–517, 530, 539) and 4 doc comments (`prompts.rs:24/171/264`, `llm.rs:11`).

## Frozen contracts touched

- **config-format** (config keys + onnx.yaml verification wording) — parity confirmed by the
  existing effective-config machine-diff tests (the corrected key list is what those tests
  already parse).
- **cli-surface** (global flags + load-test flags) — parity confirmed by the existing
  `--help` machine-diff fixtures and the load-test report fixture (which already records
  `seed: 42`).
- **data-schema** / **db-storage** / **knowledge-graph** (table placement) — parity confirmed by
  the init migrations (`migrations/knowledge/1-init/up.sql`, `migrations/cache/1-init/up.sql`)
  and the existing DAO/queue tests.
- **vector-index** (vestigial fields) — parity confirmed by the existing config-parsing tests
  in `crates/config`.

The corrections align the spec text to behavior the tests already pin; no test's expectation
changes except the NER template re-sync (task 7), which is the sanctioned exception.

## Non-goals

- **No behavior changes.** No code path, CLI flag, config key, DB table, or MCP tool changes
  behavior; the edits only make the spec text (and one AGENTS.md line) match reality.
- **No new features.** Nothing is added to the product.
- **No wire/CLI/data-contract changes.** The MCP tool set, the CLI surface, the data schema, and
  the config formats are unchanged in behavior — only the spec prose is corrected.
- **No re-opening of the docs migration.** That change is archived; this change stands alone.
- **No edit to the shipped config presets or the `edtech` demo dataset.**
- **The single sanctioned exception:** the NER template re-sync (task 7) changes one embedded
  default file + 3 test assertions + 4 doc comments so the embedded default matches the
  already-shipped config, restoring the byte-identity claim. This is a template re-sync, not a
  behavior change (the shipped config is the source of truth; the embedded default is brought to
  match it).
