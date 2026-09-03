# Tasks: post-archive-cleanups

Order reflects the dependency graph: the three tasks are independent and may
be implemented in any order, but they are listed 1.1 → 2.1 → 3.1 for a
sequential pass. Each is self-contained for a fresh agent.

## 1

- [ ] 1.1 Fix the loadtest filler's FTS bookkeeping to match the init migration

**Goal.** The `load-test` filler recreates the `chunks_fts` triggers against the
wrong column and inserts chunks without populating `search_text`, while the
consolidated init migration builds the FTS5 index **over `search_text`**. As a
result the loadtest's lexical leg indexes empty text and the recreated triggers
reference a column the FTS table does not have. Make the filler match the
migration.

**Scope (exact files).** `crates/cli/src/loadtest/filler.rs` only. Do not touch
`generator.rs` (the loadtest `Chunk` stays text-only: for synthetic chunks
`search_text == text` by construction) or any other file.

**Dependencies.** None (the `search_text` column already exists from the
`chunk-metadata-persistence` / `search-text-embedding` work; the init migration
`migrations/knowledge/1-init/up.sql:191-207` is the reference).

**What to change.**
1. In `fill_scalar_tables`, the `INSERT INTO chunks` (currently
   `(id, doc_id, chunk_text, sequence_num, start_offset, end_offset)`) gains a
   `search_text` column, set to the same value as `chunk_text` (`c.text`) —
   synthetic chunks have no heading breadcrumb, so `search_text == chunk_text`.
2. The three recreated triggers — `chunks_fts_ai`, `chunks_fts_ad`,
   `chunks_fts_au` — reference `search_text` instead of `chunk_text`
   (`new.search_text` / `old.search_text`), matching
   `migrations/knowledge/1-init/up.sql:197-207`.

**Acceptance (machine-checkable).**
- `cargo fmt --all --check` clean.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace` green.
- The `INSERT INTO chunks` in the filler includes `search_text`; the three
  triggers reference `search_text` (grep-able in the file).
- (Behavioral, if a loadtest entry point is cheap to drive in-test:) after
  `fill`, a lexical search over a known synthetic chunk's text returns that
  chunk — i.e. the FTS index is non-empty. If wiring a real search is not
  practical in the cli crate's test harness, a unit test asserting the trigger
  SQL strings reference `search_text` (and not `chunk_text`) and that the
  `INSERT` column list includes `search_text` is an acceptable alternative.

**Oracle reference.** The loadtest is Rust-only developer tooling (no Go
oracle). The schema source of truth is
`migrations/knowledge/1-init/up.sql:191-207` (FTS table over `search_text` +
its `ai`/`ad`/`au` triggers).

## 2

- [ ] 2.1 Correct the stale `document_jobs` migration reference in the data-schema spec

**Goal.** The `data-schema` main spec's `document_jobs` requirement points at a
migration path that does not exist. Correct it to the real, consolidated
migration.

**Scope (exact files).** `openspec/specs/data-schema/spec.md` only — the
requirement `### Requirement: Таблица document_jobs (очередь индексации)`.

**Dependencies.** None.

**What to change.** In that requirement, replace both references to
`migrations/knowledge/2-document-jobs/up.sql` with the consolidated
`migrations/knowledge/1-init/up.sql`:
1. In the requirement body, the phrase "создаваемой forward-only миграцией
   `migrations/knowledge/2-document-jobs/up.sql`" → "создаваемой consolidated
   init-миграцией `migrations/knowledge/1-init/up.sql`" (note the table is
   folded into the init migration).
2. In the `#### Scenario: Применение миграции` THEN clause, "миграция
   `2-document-jobs` создаёт таблицу и индекс один раз" → "init-миграция
   `1-init` создаёт таблицу и индекс один раз".
The table shape, columns, due index, and idempotency wording are unchanged.

**Acceptance (machine-checkable).**
- `openspec validate --specs` green.
- `grep -c "2-document-jobs" openspec/specs/data-schema/spec.md` == 0.
- The requirement still references `migrations/knowledge/1-init/up.sql`
  (grep-able).

**Oracle reference.** `ls migrations/knowledge/` (shows only `1-init`); the
init migration `migrations/knowledge/1-init/up.sql` contains the
`document_jobs` table and `idx_document_jobs_due` index.

## 3

- [ ] 3.1 Expose document `updated_at` on the MCP `search` result item

**Goal.** The enricher already normalizes each document's `updated_at` to
RFC3339 into the result's enrichment bag, but the MCP `search` response does
not surface it. Add an `updated_at` field to the search result item (omitted
when absent) so a RAG client can see hit freshness. This is a deliberate,
additive divergence from the Go oracle's wire item (which does not expose it),
matching the `metadata` (chunk bag) divergence from
`chunk-metadata-persistence`.

**Scope (exact files).**
- `crates/mcp/src/tools/search.rs` — the `ResultItem` struct + the
  `result_item()` mapping.
- `crates/parity-harness/src/content_parity.rs` — the response normalizer (add
  a `search` branch that strips `updated_at`, mirroring the existing
  `catalog_documents` timestamp stripping).
- (Spec delta for `mcp-contract` is authored in this change's
  `specs/mcp-contract/spec.md`; do not edit the main spec directly.)

**Dependencies.** None new (the enricher's `updated_at` already exists —
`crates/search/src/enrich.rs:103-107`).

**What to change.**
1. In `crates/mcp/src/tools/search.rs`:
   - Add `updated_at: Option<String>` to `struct ResultItem` with
     `#[serde(skip_serializing_if = "Option::is_none")]` and a doc comment.
   - In `result_item()`, populate it from
     `result.metadata.get("updated_at").and_then(Value::as_str).map(str::to_owned)`.
   - Update the existing unit tests: the canned result's enrichment bag should
     carry an `updated_at` (RFC3339) so a test asserts the field appears on the
     wire; extend the "empty/omitted" test to assert `updated_at` is omitted
     when the bag has no `updated_at`.
2. In `crates/parity-harness/src/content_parity.rs`:
   - Add a `search` normalization branch (e.g. `normalize_search`) that removes
     the `updated_at` key from each element of `results` before the strict
     `json_diff` (same treatment `normalize_catalog_documents` gives
     `updated_at`/`created_at`). Add a unit test for the strip.
   - Do **not** re-pin `fixtures/content/search.json` (the field is normalized
     away, so the Go-recorded fixture stays valid).

**Acceptance (machine-checkable).**
- `cargo fmt --all --check` clean.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace` green (including the new `search.rs` unit tests and
  the new `content_parity.rs` normalization test).
- `cargo test -p parity-harness` green (content parity + latency gates) —
  confirming the `updated_at` strip keeps the strict diff clean.
- The `ResultItem` serializes `updated_at` when `Some` and omits it when `None`
  (unit-asserted).

**Oracle reference.** `../synopsis/internal/search/enricher.go:86-91` (the Go
enricher normalizes `updated_at` to RFC3339 into `r.Metadata`) and
`../synopsis/internal/mcp/handlers/search.go:137-147` (the Go wire item that
deliberately omits it — the basis for the justified divergence).
