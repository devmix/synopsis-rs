# Proposal: post-archive-cleanups

Three small, independent follow-ups surfaced while archiving
`chunk-metadata-persistence` (and one pre-existing latent bug). All are
low-risk and isolated; none adds a dependency or changes the product
dependency graph.

## Summary

1. **Loadtest filler FTS bug (code fix).** The `load-test` filler recreates the
   `chunks_fts` triggers against the `chunk_text` column and inserts chunks
   without populating `search_text`, but the consolidated init migration builds
   the FTS5 index **over `search_text`** (`migrations/knowledge/1-init/up.sql`).
   Consequence: the loadtest's lexical leg indexes empty text (the `rebuild`
   reads `search_text`, which is `NULL`/`''` for every synthetic chunk), and the
   recreated triggers reference a column the FTS table does not have. Fix the
   filler to match the migration.

2. **Stale data-schema reference (docs fix).** The `data-schema` main spec's
   `document_jobs` requirement points at `migrations/knowledge/2-document-jobs/up.sql`,
   but the Rust migration layout is a single consolidated
   `migrations/knowledge/1-init/up.sql` (the `document_jobs` table is folded into
   it). Correct the reference.

3. **Expose document freshness on the search wire (MCP contract).** The enricher
   already normalizes each document's `updated_at` to RFC3339 into the result's
   enrichment bag, but the MCP `search` response does not surface it. Add an
   `updated_at` field to the search result item (omitted when absent) so a RAG
   client can see how fresh a hit is.

## Capabilities

- `data-schema` — correct the `document_jobs` migration reference (Task 2).
- `mcp-contract` — the `search` result item carries `updated_at` (Task 3).

(Task 1 is a bug fix inside the `cli` loadtest tooling; it changes no frozen
contract and has no spec delta.)

## Frozen contracts touched

- **mcp-contract (Task 3):** the `search` tool's result item gains an additive
  `updated_at` field. The Go oracle's MCP wire item
  (`../synopsis/internal/mcp/handlers/search.go:137-147`) does **not** expose
  `updated_at` per result (it only exposes `domains` and `entities`, extracted
  from the internal `Metadata` map). This is a deliberate, justified divergence
  — the same pattern as the `metadata` (chunk bag) field added by
  `chunk-metadata-persistence`. **Parity is preserved** because the
  parity-harness normalizer strips `updated_at` from the `search` response
  before comparison (the same treatment `catalog_documents` already gets for
  its non-deterministic timestamps), so the strict `json_diff` is unaffected.
- **data-schema (Task 2):** no behavior change — a stale path reference is
  corrected to the real migration layout.
- **cli-surface (Task 1):** no contract change — the loadtest is developer
  tooling; only its internal FTS bookkeeping is corrected.

## Non-goals

- Do **not** expose the Go oracle's other document-level `Metadata` keys
  (`document_source_type`, `document_metadata_json`) or the reranker flags
  (`is_deprecated` / `is_official` / `valid_to`) on the wire. Only the single
  most useful field — `updated_at` — is added; the rest stay in the enrichment
  bag. (User decision: "no need to specifically copy the Go keys".)
- Do **not** re-pin the `search.json` parity fixture: the non-deterministic
  `updated_at` is normalized away instead (matches the `catalog_documents`
  precedent and avoids a flaky fixture).
- Do **not** change the loadtest `Chunk` data model (`search_text == text` holds
  by construction for synthetic chunks, so no new field is added).
- Do **not** touch the Go oracle (`../synopsis`) or the production ingestion /
  search paths — only the loadtest filler, one spec reference, and the MCP
  `search` response mapping.
