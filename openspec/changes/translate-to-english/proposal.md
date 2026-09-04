# Proposal: translate-to-english

## Why

The project was developed with a Russian context, so much of its prose is in
Russian: the `openspec/config.yaml` project context, all 14 main contract specs
(`openspec/specs/**/spec.md`), the 5 ADRs (`docs/adr/*.md`), the `.opencode/`
agent + plan definitions, and ~26 Rust comment lines (criterion label chars а–л
and 3 spec-name cross-references). The rest of the repo (code, `README.md`,
`AGENTS.md`) is already English. For a consistent, autonomous repo — and ahead of
the legacy Go project's deletion — the prose should be English throughout.

This is a **language-only** change: it translates Russian prose to English and
leaves all behavior, functional data, and test fixtures untouched.

## What

- **Translate `openspec/config.yaml`** project context (Russian → English).
- **Translate the 5 ADRs** (`docs/adr/0001-sqlite-fts5.md`,
  `0002-onnx-runtime.md`, `0003-ann-engine.md`, `0004-usearch-lsm-segments.md`,
  `spike-s3-results.md`).
- **Translate the 14 main contract specs** (`openspec/specs/**/spec.md`), in two
  batches (A: 7, B: 7).
- **Translate `.opencode/agents/{orchestrator,rust-implementer,rust-reviewer}.md`
  + `.opencode/plans/db-connection-concurrency.md`.**
- **Normalize ~26 Rust comment lines:** criterion label chars (а/б/г/д/е/ж/з/и/к/л
  → the same-position Latin letter) and 3 spec-name cross-references updated to the
  translated section names (`onnx.rs` "Отсутствующий onnx.yaml" → "Missing
  onnx.yaml"; `server.rs` + `server_units.rs` "Набор инструментов" → "Tool set").

**Frozen contracts:** NO behavior change. The contract specs (mcp-contract,
cli-surface, data-schema, config-format) are translated **language-only** — every
requirement, scenario, tool name, CLI flag, schema field, and config key stays
identical in meaning. Only the prose language and the section *headings* change.
The 3 code cross-references to section names are updated to match. Parity is
confirmed by the gates (fmt/clippy/test green) and by the fact that no
requirement/scenario semantics change (the specs fix behavior as-is; only the
language of the prose moves).

## Non-goals

- **NOT functional data.** The ontology XML
  (`workspace/datasets/edtech/ontology/**` + `crates/config/tests/data/**`) keeps
  its Russian `description` / `<synonym>` values — they are the ontology's
  functional purpose (linking Russian text to entities). The demo corpus
  (`workspace/datasets/edtech/content/**`) is untouched. Rust **test data** (string
  literals like "Стив Джобс", `"я".repeat()`, "Раздел", "Первый заголовок") is
  untouched — realistic RAG test input; translating it changes what the tests
  exercise.
- **NOT `openspec/changes/archive/**`** (historical audit trail).
- **NOT code logic, public API, dependencies, or schema** — comments/docs only.
- **NOT a behavior change to any frozen contract** (language-only).
- **NOT the chunker comments that quote the unchanged test data** (e.g.
  `json.rs` "'я' is 2 bytes", `markdown.rs` 'Header "## Раздел"') — those are
  already English and the Cyrillic there is a data reference, not prose.
