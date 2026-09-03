## Why

The `search_text` change (already shipped) gave both search legs the section context (breadcrumb + body) by folding the breadcrumb into the text the legs operate on. That fixed retrieval, but it left two problems:

1. **The returned chunk's `text` field carries the synthetic breadcrumb, not the pure body.** A caller reading `result.text` gets `"Atlas Guide\n\n<body>"` — the breadcrumb is glued into the body. The pure body is `chunk_text`, which is stored but never surfaced. The section context the caller actually wants (which heading this chunk sits under) is not available as structured data anywhere in the response.

2. **The chunk's structured metadata is computed at ingestion but thrown away.** Each chunk carries a metadata bag (`section_title`, `heading_level`, `breadcrumb`, `image_paths`) in memory; the ingester persists only the document's `extra` bag to `documents.metadata_json` and drops the chunk bag entirely. The Go oracle's `SearchResult.Metadata` field is likewise dropped by the Rust port (the Rust `SearchResult` has no `metadata` field at all). So the section context that the chunker already computed is lost the moment the chunk is written.

`DocumentChunk.metadata` is currently typed `DocumentMetadata` (the document-level struct: `source_type`, `source_file`, `file_size`, `modified_at`, `extra`), which is a modeling mistake: a chunk should carry its own free-form metadata bag, not a clone of the document's typed struct. This is the root of both problems — the chunk has no natural home for its own metadata, so it is dropped.

## What Changes

- **Type-hygiene:** `DocumentChunk.metadata` changes from `DocumentMetadata` to a free-form `Map<String, Value>` — the chunk's own metadata bag. The three chunkers build this bag directly (replacing the current "clone the document's `DocumentMetadata` + insert into `extra`" pattern); NER reads the bag directly. No change to chunk boundaries, text, `search_text`, or the byte-offset invariant.
- **BREAKING (data schema, explicit decision):** the `chunks` table gains a `metadata_json TEXT` (nullable) column, folded into the single squashed init migration `migrations/knowledge/1-init/up.sql`. The ingester serializes each chunk's metadata bag into it. This deviates from the oracle's v5 shape (the Go `chunks` table has no metadata column) — justified as restoring a field that was in the original Rust design and surfacing the section context in search (see design D1). The Rust DB is always built from scratch, so there is no legacy-data migration.
- **Search:** the result `text` field becomes the pure `chunk_text` (the byte-offset slice), not `search_text`; the section context moves from the text into the structured `metadata` field. Both legs still operate on `search_text` for matching (FTS index and embeddings are unchanged) — only the *returned* text and the new `metadata` field change.
- **MCP:** the `search` tool's result item gains a `metadata` field carrying the chunk's metadata bag (raw, uncured). This closes the parity gap where the Go oracle's `SearchResult.Metadata` is dropped by the Rust port.

## Capabilities

### New Capabilities
<!-- none -->

### Modified Capabilities
- `data-schema`: the `chunks` table gains a `metadata_json TEXT` (nullable) column storing the per-chunk metadata bag; this is an explicit, justified deviation from the oracle v5 shape (the Rust DB is built from scratch, no legacy migration).
- `parsing-and-chunking`: `DocumentChunk.metadata` becomes a free-form `Map<String, Value>` (the chunk's own bag) instead of a clone of the document's `DocumentMetadata`; the chunkers build the bag directly.
- `pipeline`: the per-document pipeline persists each chunk's metadata bag to `chunks.metadata_json`; NER reads the bag directly (same keys as before).
- `hybrid-search`: the result `text` field is the pure `chunk_text`; the chunk's metadata bag is carried on the result and the legs still operate on `search_text` for matching.
- `mcp-contract`: the `search` tool's result item gains a `metadata` field carrying the chunk's metadata bag (the Go oracle's `SearchResult.Metadata`, previously dropped).

## Impact

- **Code:** `crates/ingestion` (chunk type + 3 chunkers + ingester write path + NER call site), `crates/db` (`Chunk`/`ChunkDao`/`SELECT_CHUNK`/`FTS_QUERY`/row mapping), `crates/search` (hit + result types, legs), `crates/mcp` (`search` tool result item). The `metadata_json` column is folded into the consolidated init migration (`migrations/knowledge/1-init/up.sql`).
- **Frozen contracts:** the data schema is the one explicit change (justified: restores a field from the original Rust design and surfaces section context in search). The MCP `search` result gains a `metadata` field (closes an existing parity gap; the `text` field's type is unchanged — only its content becomes the pure body). No CLI or config-format contract changes.
- **Parity:** search identity (rank order) is unaffected — both legs still match on `search_text`, so the `parity-fixture-expansion` task 1.5 rank order is unchanged. The `text` field content changes (pure body) and a `metadata` field is added; the parity harness `normalize_search` already strips `text` and compares only `{document_id, chunk_id}` + `total_count`, so the fixture's rank assertions are unaffected. The `search.json` fixture is re-pinned for cleanliness (its `text` values become the pure body).
- **No new dependencies.**

## Non-goals

- Not changing the chunking **boundaries**, `max_chunk_size`, or overlap.
- Not changing what the legs **match on** (the FTS index and the embeddings still operate on `search_text`).
- Not touching the NER provider logic (regex/llm/composite) — only the call site changes from `&chunk.metadata.extra` to `&chunk.metadata`.
- Not adding the Go oracle's *document-level* metadata keys (`document_source_type`, `updated_at`, `document_metadata_json`) to the wire `metadata` field — those are a separate, deferred concern (the wire `metadata` carries the chunk's own bag only).
- Not altering the ANN engine, RRF constants, rerank boosts, or the MCP/CLI surface.
- Not migrating or opening any legacy Go `knowledge.db`.
