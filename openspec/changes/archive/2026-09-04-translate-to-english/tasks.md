# Tasks: translate-to-english

Read first: `proposal.md`, `design.md`, and `openspec/config.yaml`. This change is
**language-only**: it translates Russian PROSE to English and leaves behavior,
functional data, and test fixtures untouched. Every task is self-contained for a
fresh agent (~100k context): goal, exact file scope, dependencies, and
machine-checkable acceptance all fit in the body.

## Rule (summary — full detail in design.md)

- **TRANSLATE (prose → natural technical English, design D5):** `config.yaml`,
  the 14 specs, the 5 ADRs, the `.opencode/` agent + plan files, and Rust comments.
  Keep identifiers, code spans, crate/tool names, ADR numbers, file paths, and
  `D1…D8` / `ADR 0001…` refs unchanged.
- **DO NOT translate (functional data):** ontology XML
  (`workspace/datasets/edtech/ontology/**` + `crates/config/tests/data/**`), demo
  corpus (`workspace/datasets/edtech/content/**`), and Rust **test-data string
  literals** (e.g. "Стив Джобс", `"я".repeat()`, "Раздел", "Первый заголовок").
  `openspec/changes/archive/**` is NOT touched.
- **Fixed section names (design D3):** `mcp-contract` "Набор инструментов" →
  **"Tool set"**; `config-format` "Отсутствующий onnx.yaml" → **"Missing
  onnx.yaml"**.
- **Criterion labels (design D4):** Cyrillic → same-position Latin: а→a, б→b, в→c,
  г→d, д→e, е→f, ж→g, з→h, и→i, к→j, л→l.
- **Cyrillic detection:** the range `[\x{0400}-\x{04FF}]`. Whole-file check:
  `git grep -c -P '[\x{0400}-\x{04FF}]' -- <file>` → **0**.
- **Gates:** `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --
  -D warnings`, `cargo test --workspace` stay green. (Docs/config-only tasks have
  no cargo impact but must not break the YAML.)

## 1 — Config + ADRs

- [x] 1.1 Translate `openspec/config.yaml`

**Goal.** Translate the project-context prose in `openspec/config.yaml` from
Russian to natural technical English. Keep every YAML key, list structure, and
value *meaning* identical — only the language of the prose values moves. Keep
technical terms, crate names, tool names, `D1…D8` / `ADR 0001…` refs, file paths,
and code spans unchanged.

**Scope (exact file).** `openspec/config.yaml` (the whole file — the `project`
context block and the `rules` block are both Russian prose under English keys).

**Acceptance.** `git grep -c -P '[\x{0400}-\x{04FF}]' -- openspec/config.yaml` →
**0**. The YAML still parses: `python3 -c "import yaml; yaml.safe_load(open('openspec/config.yaml'))"`
→ no error, and the top-level keys (`project`, `rules`, and any others) are
unchanged. No key renamed; only prose values translated.

- [x] 1.2 Translate the 5 ADRs

**Goal.** Translate the 5 ADR documents from Russian to natural technical English,
preserving every decision, rationale, consequence, and cross-reference in meaning.
Keep ADR numbers, file links, spec/section names that are NOT in the D3 fixed set,
crate/tool names, code spans, and `D1…D8` / `ADR 0001…` refs unchanged.

**Scope (exact files).** `docs/adr/0001-sqlite-fts5.md`,
`docs/adr/0002-onnx-runtime.md`, `docs/adr/0003-ann-engine.md`,
`docs/adr/0004-usearch-lsm-segments.md`, `docs/adr/spike-s3-results.md`.

**Acceptance.** `git grep -c -P '[\x{0400}-\x{04FF}]' -- docs/adr/0001-sqlite-fts5.md docs/adr/0002-onnx-runtime.md docs/adr/0003-ann-engine.md docs/adr/0004-usearch-lsm-segments.md docs/adr/spike-s3-results.md`
→ **0** (all five files). ADR↔ADR cross-references (e.g. ADR 0003 →
spike-s3-results.md) and ADR→spec references remain valid (same ADR numbers, same
target file/section). `docs/adr/README.md` and `docs/adr/template.md` are already
English — do not touch them.

## 2 — Contract specs (two batches)

- [x] 2.1 Translate specs batch A (7 specs)

**Goal.** Translate 7 main contract specs from Russian to natural technical
English, preserving every requirement, scenario, and Given/When/Then exactly in
meaning. Translate section headings (design D2). Apply the D3 fixed name to
`config-format`: "Отсутствующий onnx.yaml" → **"Missing onnx.yaml"**.

**Scope (exact files).** `openspec/specs/cli-surface/spec.md`,
`openspec/specs/config-format/spec.md`, `openspec/specs/data-schema/spec.md`,
`openspec/specs/db-storage/spec.md`, `openspec/specs/embedding/spec.md`,
`openspec/specs/entity-extraction/spec.md`, `openspec/specs/hybrid-search/spec.md`.

**Acceptance.** `git grep -c -P '[\x{0400}-\x{04FF}]' --` over those 7 files →
**0** (each). The requirement/scenario COUNT and structure are unchanged (same
number of `### Requirement:` and `#### Scenario:` headings as before — verify with
`grep -c` before/after if unsure). `openspec/specs/config-format/spec.md` now
contains "Missing onnx.yaml" and no "Отсутствующий". No tool name, CLI flag, schema
field, or config key changed in meaning.

- [x] 2.2 Translate specs batch B (7 specs)

**Goal.** Translate the remaining 7 main contract specs from Russian to natural
technical English, preserving every requirement, scenario, and Given/When/Then
exactly in meaning. Translate section headings (design D2). Apply the D3 fixed
name to `mcp-contract`: "Набор инструментов" → **"Tool set"**.

**Scope (exact files).** `openspec/specs/knowledge-graph/spec.md`,
`openspec/specs/llm-client/spec.md`, `openspec/specs/mcp-contract/spec.md`,
`openspec/specs/parsing-and-chunking/spec.md`, `openspec/specs/pipeline/spec.md`,
`openspec/specs/utils/spec.md`, `openspec/specs/vector-index/spec.md`.

**Acceptance.** `git grep -c -P '[\x{0400}-\x{04FF}]' --` over those 7 files →
**0** (each). Requirement/scenario count and structure unchanged.
`openspec/specs/mcp-contract/spec.md` now contains "Tool set" and no "Набор
инструментов". The 12 frozen tool names, their argument/result shapes, and all
`D1…D8` / `ADR 0001…` refs are unchanged in meaning.

## 3 — Agents/plans + Rust comments

- [x] 3.1 Translate `.opencode/` agent + plan files and the Rust comment lines

**Goal.** (a) Translate the remaining Russian in the 3 agent files + the plan file
to English (mostly field-name references like "Scope файлов" → "File scope",
"Критерии приёмки" → "Acceptance criteria", "Зависимости" → "Dependencies", plus
the plan's Russian prose). (b) Normalize the ~26 Rust comment lines: criterion
label chars (design D4) and the 3 spec-name cross-references (design D3). Do NOT
touch Rust test-data string literals or the chunker comments that quote unchanged
test data.

**Dependencies.** Tasks 2.1 and 2.2 must be complete first (the Rust spec-name refs
must match the already-translated spec section names "Missing onnx.yaml" and
"Tool set").

**Scope (exact files).**
- `.opencode/agents/orchestrator.md`, `.opencode/agents/rust-implementer.md`,
  `.opencode/agents/rust-reviewer.md`, `.opencode/plans/db-connection-concurrency.md`.
- Rust comment lines (comments only — do NOT change any code, string literal, or
  test assertion):
  - `crates/config/src/domain.rs:415` — `(б)` → `(b)`
  - `crates/config/src/onnx.rs:12` — "Отсутствующий onnx.yaml" → "Missing onnx.yaml"
  - `crates/db/src/chunk.rs:828` `(д)`→`(e)`, `:904` `(е)`→`(f)`
  - `crates/db/src/document.rs:691` — `(з)` → `(h)`
  - `crates/db/src/entity_link.rs:460` — `(г)` → `(d)`
  - `crates/db/src/entity_source.rs:277` `(и, cont.)`→`(i, cont.)`, `:331`
    `(к, cont.)`→`(j, cont.)`, `:398` `(л, cont.)`→`(l, cont.)`
  - `crates/mcp/src/server.rs:325` — "Набор инструментов" → "Tool set"
  - `crates/mcp/tests/server_units.rs:20` — "Набор инструментов" → "Tool set"
  - `crates/config/tests/domain.rs` — criterion labels `(а)(б)(г)(д)(е)` → `(a)(b)(d)(e)(f)` (lines ~10, 61, 118, 172, 218, 248, 270, 286)
  - `crates/config/tests/ontology.rs` — criterion labels `(а)(б)(г)(д)(ж)(з)` → `(a)(b)(d)(e)(g)(h)` (lines ~11, 12, 15, 225, 240, 259)
  - `crates/config/tests/preset.rs:323` — `(г)` → `(d)`

**Acceptance.**
- `git grep -c -P '[\x{0400}-\x{04FF}]' -- .opencode/agents/orchestrator.md .opencode/agents/rust-implementer.md .opencode/agents/rust-reviewer.md .opencode/plans/db-connection-concurrency.md` → **0** (all four files).
- Spec names gone from code: `git grep -n 'Набор инструментов\|Отсутствующий onnx.yaml' -- crates/` → **0**.
- Criterion labels gone from comments: `git grep -n -P '(//|///|//!).*\([а-я]' -- crates/` → **0**.
- Test data UNCHANGED: `git grep -c '"Стив Джобс"' -- crates/` → still **> 0** (the Russian test literals remain).
- Gates green: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` (comments-only change; no test behavior changes).

## 4 — Verification

- [x] 4.1 Final whole-repo verification

**Goal.** Confirm the repo prose is fully English outside the excluded functional
data, and all gates are green.

**Acceptance.**
- Whole-repo Cyrillic sweep (excl. the excluded functional data + archive) → **0**:
  `git grep -l -P '[\x{0400}-\x{04FF}]' -- ':!openspec/changes/archive/**' ':!.archive/**' ':!workspace/datasets/edtech/content/**' ':!workspace/datasets/edtech/ontology/**' ':!crates/config/tests/data/**' ':!*.png' ':!*.jpg' ':!*.jpeg' ':!*.gif' ':!openspec/changes/translate-to-english/**' 2>/dev/null | grep -v node_modules | grep -v '/target/'`
  → the ONLY remaining files are the Rust files that still hold **test-data**
  literals (`crates/ingestion/src/**` test modules, `crates/config/tests/domain.rs`,
  `crates/config/tests/ontology.rs` — the data, not the comments). Confirm none of
  the remaining hits are in a `//`/`///`/`//!` comment that is prose (the
  chunker comments quoting 'я'/'Раздел'/'Первый заголовок' are allowed data refs).
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo test --workspace` all green.
- The 2 spec section names are consistent: `openspec/specs/mcp-contract/spec.md`
  and `crates/mcp/src/server.rs` + `crates/mcp/tests/server_units.rs` all use
  "Tool set"; `openspec/specs/config-format/spec.md` and
  `crates/config/src/onnx.rs` all use "Missing onnx.yaml".
- Archive untouched: `git status --porcelain -- openspec/changes/archive/` → empty.
