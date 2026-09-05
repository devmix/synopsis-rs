# Tasks — sync-specs-to-implementation

## How this change is applied

The six spec deltas are **authored in this draft** under `specs/<capability>/spec.md`. Applying the
change means: (a) confirm each delta is faithful to the current main spec (requirement name match,
full corrected text, valid scenarios preserved, only the stale parts changed); (b) make the one
AGENTS.md line edit (task 6); (c) make the one code change — the NER template re-sync (task 7);
then (d) `opsx-sync` merges the six deltas into `openspec/specs/` and the final gates run (task 8).
Tasks 1–5 are therefore the per-capability delta confirmations; task 8 is the sync + verify.

Order (dependency graph): 1, 2, 3, 4, 5 are independent; 6, 7, 9 are independent; 8 depends on all
of 1–7. No task starts before its dependencies are complete.

---

### Task 1 — config-format delta (legacy-key removal + onnx checksum wording)

**Goal.** Confirm the `config-format` delta is faithful to the current main spec and ready for
`opsx-sync`. The delta is already authored at
`openspec/changes/sync-specs-to-implementation/specs/config-format/spec.md` (two `## MODIFIED
Requirements`).

**Exact file scope (read-only confirm).**
- Delta: `openspec/changes/sync-specs-to-implementation/specs/config-format/spec.md`
- Reference (do NOT edit): `openspec/specs/config-format/spec.md`
- Implementation references (do NOT edit): `crates/config/src/preset.rs` (`PathsConfig`,
  `DatabaseConfig`), `crates/config/src/onnx.rs` (`size_bytes`, `checksum: Option<String>`).

**Corrections to confirm present in the delta.**
1. Requirement **`Full YAML preset`** (name must match the main spec exactly): the `database`
   section lists only `(pragma)`; the `paths` section lists only
   `(workspace_dir, migrations_dir, prompts_path, onnx_config)`; the four legacy keys are absent
   from the key list entirely (removed, not mentioned); the knowledge-DB path is stated as derived
   (`<workspace_dir>/datasets/<name>/state/knowledge.db`). All pre-existing scenarios preserved;
   the derived-path scenario added.
2. Requirement **`onnx.yaml model registry`** (name must match): download verification reads
   "size check enforced; `checksum` optional and NOT verified (downloads verified by size only,
   not 'URL + size + SHA-256')". Pre-existing scenario preserved; the size/checksum scenario added.

**Dependencies.** None (independent delta).

**Acceptance (machine-checkable).**
- `openspec validate sync-specs-to-implementation` passes.
- The two requirement names in the delta equal the names in
  `openspec/specs/config-format/spec.md` (`Full YAML preset`, `onnx.yaml model registry`).
- `git grep -n 'data_dir\|documents_dir\|global_config_path\|database\.path' --
   openspec/changes/sync-specs-to-implementation/specs/config-format/spec.md` → **0** (the four
   legacy keys are absent entirely).
- The delta contains no other capability or requirement.

---

### Task 2 — cli-surface delta (--db removal + --seed)

**Goal.** Confirm the `cli-surface` delta is faithful and ready for `opsx-sync`. Delta authored at
`openspec/changes/sync-specs-to-implementation/specs/cli-surface/spec.md` (two `## MODIFIED
Requirements`).

**Exact file scope (read-only confirm).**
- Delta: `openspec/changes/sync-specs-to-implementation/specs/cli-surface/spec.md`
- Reference (do NOT edit): `openspec/specs/cli-surface/spec.md`
- Implementation references (do NOT edit): `crates/cli/src/cli.rs` (`build_command`: global args
  `config`/`preset`/`dataset`, `.version`, load-test `seed` default `"42"`, `DEFAULT_SEED`).

**Corrections to confirm present in the delta.**
1. Requirement **`Global flags and command-line structure`** (name must match): invocation format is
   `synopsis [--config PATH] [--preset NAME] [--dataset NAME] [--version] <subcommand> [flags...]`;
   `--db PATH` is gone and stated as nonexistent; globals listed as `--config`, `--preset`,
   `--dataset`, `--version`. The 2026-08-29 contract-decision note and the `Argument order` +
   `Version` scenarios preserved.
2. Requirement **`load-test subcommand`** (name must match): flag list now includes `--seed N`
   (default 42). The `Report` scenario preserved; a deterministic-seed scenario added.

**Dependencies.** None (independent delta).

**Acceptance (machine-checkable).**
- `openspec validate sync-specs-to-implementation` passes.
- The two requirement names match `openspec/specs/cli-surface/spec.md`.
- `git grep -n -- '--db' -- openspec/changes/sync-specs-to-implementation/specs/cli-surface/spec.md`
  shows `--db` only in the "no `--db` flag" sentence.
- The delta's load-test requirement lists `--seed` (default 42).

---

### Task 3 — data-schema + db-storage delta (app_kv -> cache DB)

**Goal.** Confirm the `data-schema` and `db-storage` deltas are faithful and ready for
`opsx-sync`. Deltas authored at
`openspec/changes/sync-specs-to-implementation/specs/data-schema/spec.md` and
`openspec/changes/sync-specs-to-implementation/specs/db-storage/spec.md` (one `## MODIFIED
Requirement` each).

**Exact file scope (read-only confirm).**
- Deltas: the two files above.
- References (do NOT edit): `openspec/specs/data-schema/spec.md`, `openspec/specs/db-storage/spec.md`.
- Implementation references (do NOT edit): `migrations/knowledge/1-init/up.sql` (no `app_kv`),
  `migrations/cache/1-init/up.sql` (`app_kv`, `llm_ner_cache`, `llm_linker_cache`).

**Corrections to confirm present in the deltas.**
1. `data-schema` requirement **`Compatibility with the v5 schema`** (name must match): the table
   list drops `app_kv`; a sentence states `app_kv` lives in the cache DB
   (`migrations/cache/1-init/up.sql`). The `Fresh-DB startup` + `Read-query repeatability`
   scenarios preserved; a cache-DB scenario added.
2. `db-storage` requirement **`DAO operations over the v5 schema`** (name must match): the DAO
   table list drops `app_kv`; a sentence states `app_kv` is in the cache DB. All five pre-existing
   scenarios preserved.

**Dependencies.** None (independent delta).

**Acceptance (machine-checkable).**
- `openspec validate sync-specs-to-implementation` passes.
- The requirement names match the two main specs.
- `git grep -n 'app_kv' -- openspec/changes/sync-specs-to-implementation/specs/data-schema/spec.md
  openspec/changes/sync-specs-to-implementation/specs/db-storage/spec.md` shows `app_kv` only in the
  "NOT in the knowledge DB / cache DB" sentence, not in the knowledge-DB table/DAO list.

---

### Task 4 — knowledge-graph delta (linker cache table)

**Goal.** Confirm the `knowledge-graph` delta is faithful and ready for `opsx-sync`. Delta authored
at `openspec/changes/sync-specs-to-implementation/specs/knowledge-graph/spec.md` (one `## MODIFIED
Requirement`).

**Exact file scope (read-only confirm).**
- Delta: the file above.
- Reference (do NOT edit): `openspec/specs/knowledge-graph/spec.md`.
- Implementation references (do NOT edit): `migrations/cache/1-init/up.sql` (`llm_linker_cache`),
  the linker code that keys decisions in `llm_linker_cache`.

**Correction to confirm present in the delta.**
- Requirement **`Cross-domain linking pipeline`** (name must match): the decision-cache location is
  the `llm_linker_cache` table (cache DB, `migrations/cache/1-init/up.sql`), not `app_kv`. All eight
  pre-existing scenarios preserved; the `Decision cache` scenario's THEN now names
  `llm_linker_cache`.

**Dependencies.** None (independent delta).

**Acceptance (machine-checkable).**
- `openspec validate sync-specs-to-implementation` passes.
- The requirement name matches `openspec/specs/knowledge-graph/spec.md`.
- `git grep -n 'app_kv' -- openspec/changes/sync-specs-to-implementation/specs/knowledge-graph/spec.md`
  → **0**; `git grep -c 'llm_linker_cache' --
  openspec/changes/sync-specs-to-implementation/specs/knowledge-graph/spec.md` ≥ 2 (requirement +
  Decision-cache scenario).

---

### Task 5 — vector-index delta (vestigial fields)

**Goal.** Confirm the `vector-index` delta is faithful and ready for `opsx-sync`. Delta authored at
`openspec/changes/sync-specs-to-implementation/specs/vector-index/spec.md` (one `## MODIFIED
Requirement`).

**Exact file scope (read-only confirm).**
- Delta: the file above.
- Reference (do NOT edit): `openspec/specs/vector-index/spec.md`.
- Implementation references (do NOT edit): `crates/config/src/preset.rs` (`num_partitions` /
  `nprobes` fields, serde defaults 256/32).

**Correction to confirm present in the delta.**
- Requirement **`Index configuration`** (name must match): the `num_partitions`/`nprobes` sentence
  reads that they **persist as vestigial/unused config fields** (present for tolerant parsing,
  serde defaults 256/32, unused by `UsearchEngine`) — NOT "removed with the lance engine". All five
  pre-existing scenarios preserved; a "Vestigial IVF fields tolerated" scenario added.

**Dependencies.** None (independent delta).

**Acceptance (machine-checkable).**
- `openspec validate sync-specs-to-implementation` passes.
- The requirement name matches `openspec/specs/vector-index/spec.md`.
- `git grep -n 'were removed with the lance' --
  openspec/changes/sync-specs-to-implementation/specs/vector-index/spec.md` → **0**; the delta
  contains "vestigial/unused" and "256/32".

---

### Task 6 — AGENTS.md line 17 (embeddings + NER -> embeddings only)

**Goal.** Correct the one stale AGENTS.md line so the frozen-stack line matches the implementation.
The implementation's NER is `RegexNer` (rule-based) + `LlmNer` (remote OpenAI-compatible HTTP
client via `crates/llm`); prose/statistical NER was rejected by human decision 2026-08-23. ONNX
serves **embeddings only**.

**Exact file scope (edit).**
- `AGENTS.md` line 17 only.

**Edit.**
- Current: `- ONNX runtime as external \`.so\`/\`.dylib\` (bge-m3 int8, 1024-dim embeddings + NER);
  downloaded/verified per \`onnx.yaml\`. ...`
- Change `1024-dim embeddings + NER` → `1024-dim embeddings only` (NER is rule-based `RegexNer` +
  remote `LlmNer`, not ONNX). Do NOT touch the rest of the line or any other line.

**Dependencies.** None (independent text edit).

**Acceptance (machine-checkable).**
- `git grep -n 'embeddings + NER' -- AGENTS.md` → **0**;
  `git grep -n '1024-dim embeddings only' -- AGENTS.md` → 1 (line 17).
- `git diff -- AGENTS.md` shows exactly one changed line (line 17).

---

### Task 7 — NER template re-sync (the one code change)

**Goal.** Make the embedded NER default byte-identical to the shipped config, restoring the
byte-identity claim (AGENTS.md line 70 + `workspace/configs/README.md` line 19). The shipped config
is the source of truth; the embedded default moves to match it.

**Exact file scope (edit).**
- `crates/ingestion/src/ner/templates/user.tmpl` — overwrite so it is byte-identical to
  `workspace/configs/prompts/ner/user.tmpl` (the fenced `CONTENT SECTION` format).
- `crates/ingestion/src/ner/prompts.rs` — update the 3 test assertions that assert the old
  `Document context:` format (lines ~512, 515–517, 530, 539) to assert the new fenced
  `CONTENT SECTION` format (the section path is rendered as a fenced block; the clean chunk text is
  wrapped in a fenced `CONTENT` block).
- Doc comments (4): `crates/ingestion/src/ner/prompts.rs:24/171/264` and
  `crates/ingestion/src/ner/llm.rs:11` — update the "Document context block" wording to describe the
  `CONTENT SECTION` fenced block.

**Dependencies.** None (independent code change). Task 8's final `cargo test` depends on this.

**Acceptance (machine-checkable).**
- `diff -q crates/ingestion/src/ner/templates/user.tmpl workspace/configs/prompts/ner/user.tmpl`
  → no output (byte-identical).
- `cargo test -p ingestion` passes (the 3 updated assertions now assert the new format).
- `cargo clippy -p ingestion --all-targets -- -D warnings` clean; `cargo fmt -p ingestion --check`
  clean (run workspace-wide in task 8).
- `git grep -n 'Document context' -- crates/ingestion/src/ner/prompts.rs` reflects only the
  updated doc comments (no assertion still expects the old format).

---

### Task 8 — opsx-sync + validate + gates (final)

**Goal.** Merge the six authored deltas into the main specs, then run the full verification gates.

**Exact file scope (written by `opsx-sync`; then gates).**
- `openspec/specs/{config-format,cli-surface,data-schema,db-storage,knowledge-graph,vector-index}/spec.md`
  — updated by `opsx-sync` from the deltas (each MODIFIED requirement replaces the old one by name).
- `AGENTS.md` line 17 (task 6) and the NER files (task 7) are already done.

**Steps.**
1. Run `opsx-sync sync-specs-to-implementation` to merge the six deltas into
   `openspec/specs/`. Verify each of the six requirements in the main specs now carries the
   corrected text (the four legacy keys gone, `--db` gone + `--seed` present, `app_kv` re-pointed to
   the cache DB, linker cache = `llm_linker_cache`, vestigial `num_partitions`/`nprobes`).
2. `openspec validate sync-specs-to-implementation`.
3. `cargo fmt --check`.
4. `cargo clippy --all-targets -- -D warnings`.
5. `cargo test` (full workspace, green — includes the re-synced NER tests).

**Dependencies.** Tasks 1–7 all complete.

**Acceptance (machine-checkable).**
- After sync: `git grep -n 'database.path\|data_dir\|documents_dir\|global_config_path' --
  openspec/specs/config-format/spec.md` → the keys appear only in the "removed" sentence;
  `git grep -n -- '--db' -- openspec/specs/cli-surface/spec.md` → only the "no `--db`" sentence;
  `git grep -n 'app_kv' -- openspec/specs/data-schema/spec.md openspec/specs/db-storage/spec.md`
  → only the "cache DB" sentence; `git grep -n 'app_kv' --
  openspec/specs/knowledge-graph/spec.md` → 0; `git grep -n 'were removed with the lance' --
  openspec/specs/vector-index/spec.md` → 0.
- `openspec validate sync-specs-to-implementation` passes.
- `cargo fmt --check` clean; `cargo clippy --all-targets -- -D warnings` clean; `cargo test` green.

---

### Task 9 — openspec/config.yaml frozen-stack context consistency (follow-up)

**Goal.** Correct the two drifts that `openspec/config.yaml` (the frozen-stack context shown to
AI agents) repeats from the AGENTS.md line 17 fix and the config-format onnx.yaml fix.

**Exact file scope (edit).**
- `openspec/config.yaml` lines 16–17 only.

**Edits.**
- Line 16: `embeddings (bge-m3 int8, 1024-dim) and NER` → `embeddings (bge-m3 int8, 1024-dim)
  only` (NER is `RegexNer` rule-based + remote `LlmNer`, not ONNX — matches the AGENTS.md line 17
  fix).
- Line 17: `registry: URL + size_bytes + sha256-verify` → `registry: URL + size_bytes (size
  enforced; checksum optional)` (matches the config-format onnx.yaml fix: size is the enforced
  check, the checksum field is optional and not verified).

**Dependencies.** None (independent text edit; no Rust code, so the cargo gates are unaffected —
only `openspec validate` needs re-running).

**Acceptance (machine-checkable).**
- `git grep -n 'and NER' -- openspec/config.yaml` → 0; `git grep -n '1024-dim) only' --
  openspec/config.yaml` → 1 (line 16).
- `git grep -n 'sha256-verify' -- openspec/config.yaml` → 0; `git grep -n 'size enforced; checksum
  optional' -- openspec/config.yaml` → 1 (line 17).
- `openspec validate --specs` and `openspec validate sync-specs-to-implementation` pass.
