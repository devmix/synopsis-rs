# Design: post-archive-cleanups

Three isolated decisions. Each names its Go-oracle reference (where one
applies) and why the chosen approach beats the alternative.

## D1 — Loadtest filler: match the init migration's `search_text` FTS layout

**Decision.** In `crates/cli/src/loadtest/filler.rs`, (a) the `INSERT INTO
chunks` gains a `search_text` column set to the chunk text (`c.text`), and (b)
the three recreated triggers (`chunks_fts_ai` / `chunks_fts_ad` /
`chunks_fts_au`) reference `search_text` instead of `chunk_text`.

**Why.** The consolidated init migration
(`migrations/knowledge/1-init/up.sql:191-207`) defines
`CREATE VIRTUAL TABLE chunks_fts USING fts5(search_text, …)` and its triggers
fire on `new.search_text` / `old.search_text`. The filler drops and recreates
those triggers, so the recreated ones must target the same column or they
reference a non-existent FTS column. And the `rebuild` command reads
`chunks.search_text`, so the column must be populated or the lexical leg
indexes empty text. Synthetic loadtest chunks have no heading breadcrumb, so
`search_text == chunk_text == c.text` by construction — no data-model change
needed.

**Why not** add a `search_text` field to the loadtest `Chunk` struct
(`generator.rs`): it would be pure duplication (always equal to `text`),
violating DRY for zero benefit.

**Oracle reference.** The Go oracle's loadtest does not exist (the loadtest is
Rust-only developer tooling); the reference is the Rust init migration itself,
which is the schema source of truth.

## D2 — data-schema: correct the `document_jobs` migration reference

**Decision.** Rewrite the two references to
`migrations/knowledge/2-document-jobs/up.sql` in the `data-schema` spec's
`document_jobs` requirement (body + scenario) to point at the consolidated
`migrations/knowledge/1-init/up.sql`.

**Why.** `ls migrations/knowledge/` shows a single `1-init` directory; the
`document_jobs` table and its `idx_document_jobs_due` index are folded into the
init migration (the forward-only `2-document-jobs` migration was consolidated
during the native-seam-spikes D6 squash). The spec's *behavior* (table shape,
columns, due index, idempotency) is correct and unchanged — only the path was
stale.

**Why not** a code change: there is none to make; this is a documentation
correction in the main spec.

## D3 — MCP `search` wire: expose `updated_at` (deliberate contract divergence)

**Decision.** Add `updated_at: Option<String>` to the MCP `search` result item
(`crates/mcp/src/tools/search.rs`), serialized with
`#[serde(skip_serializing_if = "Option::is_none")]`, sourced from the
enricher's `result.metadata["updated_at"]` (already RFC3339-normalized). The
parity-harness normalizer gains a `search` branch that strips `updated_at` from
each result item before the strict `json_diff`.

**Why `updated_at`.** It is the single most useful document-level attribute for
a RAG client (freshness / staleness) and is already computed in the enrichment
bag (`enrich.rs:103-107`); surfacing it is pure plumbing. The user directed
"add useful information; no need to specifically copy the Go keys", so only the
one high-value field is added — not the raw `document_metadata_json` or the
niche reranker flags.

**Why a deliberate divergence (frozen contract).** The Go oracle's MCP wire item
(`../synopsis/internal/mcp/handlers/search.go:137-147`) exposes
`document_id, chunk_id, text, sequence_num, start_offset, end_offset,
document_path, score, source_type, domains, entities` — **no** `updated_at` and
**no** `Metadata` map. Exposing `updated_at` is therefore an additive Rust-only
enhancement, the same category as the `metadata` (chunk bag) field added by
`chunk-metadata-persistence` (which the Go wire also lacks). Per the migration
principles, a frozen-contract change is allowed when Rust is strictly better,
with explicit justification: a client can now see hit freshness without a
second `get_document_context` call.

**Why strip in the normalizer (not re-pin the fixture).** `updated_at` is
non-deterministic (it derives from the fixture DB's document timestamps), so a
re-pinned fixture would be flaky. The parity harness already strips
non-deterministic timestamps for `catalog_documents`
(`content_parity.rs:185-199`); extending the same treatment to the `search`
response is the consistent, stable choice. The `updated_at` field's presence
and RFC3339 shape are validated by unit tests instead.

**Why not** also expose the reranker flags (`is_deprecated` / `is_official` /
`valid_to`): they are niche, and the user asked not to copy the Go keys
mechanically. They remain in the enrichment bag and can be a follow-up if
needed.

**Oracle reference.** `../synopsis/internal/search/enricher.go:86-91` (the Go
enricher already normalizes `updated_at` to RFC3339 into `r.Metadata`), and
`../synopsis/internal/mcp/handlers/search.go:137-147` (the Go wire item that
deliberately omits it).
