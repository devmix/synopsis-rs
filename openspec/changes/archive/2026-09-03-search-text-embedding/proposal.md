## Why

The Rust `MarkdownChunker` keeps the **byte-offset invariant** — `content[start_offset..end_offset] == text`, so a chunk's `text` is a pure slice of the source and the heading breadcrumb lives only in `metadata`. This is a deliberate correctness fix over the oracle (which prefixed breadcrumbs/file names into `Text` while leaving the offsets at the original span). But it has a real cost for RAG retrieval: both search legs lose the section context.

- **Semantic leg:** the embedding is computed on the clean body slice, without the heading path that names the topic — a body chunk about "the migration process" is ambiguous without "Atlas API Migration > Phase 2".
- **Lexical leg:** the FTS5 index is built on the clean body slice, so heading terms that users search by are missing (in the parity corpus, "atlas" appears in chunk 25 *only* via its breadcrumb, so the Rust FTS leg misses a chunk the oracle matches).

The Go oracle feeds both legs the breadcrumb-prefixed text, so its search identity and rank order differ from Rust's. This was surfaced by `parity-fixture-expansion` task 1.5: for the query "Atlas dashboard builder", Go top-5 = `[18,25,33,17,3]` vs Rust top-5 = `[18,33,17,25,23]` (top-1 agrees, positions 2-5 diverge). The search pipeline (RRF, HNSW, normalization, rerank) is a verified-correct port; the divergence is purely the chunker's documented deviation changing both legs' inputs.

For RAG quality the section context is a net positive for both legs, so the correct fix is to give the search legs the breadcrumb-prefixed text **without** breaking the byte-offset invariant.

## What Changes

Introduce a per-chunk `search_text = breadcrumb + "\n\n" + body` (or just `body` when there is no breadcrumb). The search legs consume `search_text`; the byte-offset invariant and NER are unchanged.

- **BREAKING (data schema, explicit decision):** the `chunks` table gains a `search_text TEXT NOT NULL` column (default = `chunk_text`); the FTS5 `chunks_fts` index is re-pointed from `chunk_text` to `search_text` (its `ai/ad/au` triggers updated). This deviates from the oracle's v5 shape — justified as a retrieval-quality improvement (see design D1). The Rust DB is always built from scratch, so there is no legacy-data migration.
- `crates/ingestion`: `DocumentChunk` gains `search_text`; the Markdown chunker builds it from the breadcrumb + body. The ingester computes the **embedding on `search_text`** and writes both `chunk_text` (clean slice) and `search_text`. **NER is untouched** (it still runs on `chunk.text` + the metadata extras; breadcrumbs are substituted in the NER template as today).
- `crates/search`: the search result's `text` field becomes `search_text`, so the returned chunk carries its section context (matching the oracle's result shape).
- `crates/db`: `Chunk` gains `search_text`; `ChunkDao::create`/`update` take it; the FTS query and `SELECT_CHUNK` use `search_text` for the index while still returning `chunk_text` for the row.

## Capabilities

### New Capabilities
<!-- none -->

### Modified Capabilities
- `data-schema`: the `chunks` table gains a `search_text` column and the FTS5 index is re-pointed to it; this is an explicit, justified deviation from the oracle v5 shape (the Rust DB is built from scratch, no legacy migration).
- `parsing-and-chunking`: the Markdown chunker emits `search_text` (breadcrumb + body) in addition to the invariant-preserving `text`.
- `pipeline`: the per-document pipeline embeds `search_text` (not `text`) and persists both fields; NER remains on `text` + metadata.
- `db-storage`: `ChunkDao` create/update carry `search_text`; the FTS5 index and its triggers operate on `search_text` while row reads still expose `chunk_text`.
- `hybrid-search`: both legs (lexical FTS5 and semantic vector) are fed `search_text`, and the fused result's `text` field is `search_text`.

## Impact

- **Code:** `crates/ingestion` (chunker + ingester), `crates/db` (`Chunk`/`ChunkDao`/FTS), `crates/search` (result `text`), the `search_text` re-point folded into the consolidated init migration (`migrations/knowledge/1-init/up.sql`; task 5.1 squashed the five forward-only migrations into one, so `rusqlite_migration` sets `user_version` to the migration count = 1).
- **Frozen contracts:** data schema is the one explicit change (justified retrieval improvement). No MCP tool, CLI, or config-format contract changes. The `search` tool's response shape is unchanged (same fields; the `text` value now carries section context).
- **Parity:** after this change the `parity-fixture-expansion` task 1.5 search identity is expected to match the Go oracle (both legs see the same breadcrumb-prefixed text), allowing the test to assert **full identity parity** without weakening `normalize`. Catalog parity (task 1.4, `chunk_count`) is unaffected (chunk boundaries do not depend on text).
- **No new dependencies.**

## Non-goals

- Not changing the chunking **boundaries** (section splitting, overlap, `max_chunk_size`) — only what text the search legs see.
- Not touching NER (it keeps using `text` + metadata; breadcrumbs are already substituted in the NER template).
- Not changing the byte-offset invariant on `chunk_text` (`content[start..end] == chunk_text` still holds).
- Not altering the ANN engine, RRF constants, rerank boosts, or the MCP/CLI surface.
- Not migrating or opening any legacy Go `knowledge.db`.
