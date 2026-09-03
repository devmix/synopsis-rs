## Context

See proposal.md for the motivation. The current state that shapes the approach:

- `DocumentChunk.metadata` is typed `DocumentMetadata` (the document-level struct: `source_type`, `source_file`, `file_size`, `modified_at`, `extra: Map<String, Value>`). Each chunk clones the document's `DocumentMetadata`; the chunkers add chunk-specific keys (`section_title`, `heading_level`, `breadcrumb`, `image_paths`) to `metadata.extra` (`crates/ingestion/src/chunkers/*`).
- The chunk's metadata bag is computed in memory but **dropped at write time**: the ingester serializes only the *document's* `doc.metadata.extra` to `documents.metadata_json` and calls `chunk_dao.create_with_search_text(doc_id, &chunk.text, &chunk.search_text, …)` with no chunk metadata (`crates/ingestion/src/ingester/mod.rs`).
- The `chunks` table has **no** metadata column (the oracle v5 shape); the original Rust design *had* a `metadata_json` column (see the note at `crates/db/src/chunk.rs:41-43`), which was dropped to match the frozen v5 schema.
- NER runs on `chunk.text` + `&chunk.metadata.extra` (`crates/ingestion/src/ingester/mod.rs:416`); the `NerProvider::extract_entities` signature already takes a `&Map<String, Value>` bag — so it consumes the bag directly today, via `.extra`.
- The search hit structs (`LexicalHit`, `SemanticHit`, `SearchResult`) each carry a `chunk_text` field that currently holds `search_text` (the naming trade-off from the `search_text` change); the result's `text` field is that value. `SearchResult` has a `metadata` field, but it is the *enrichment* bag (domains, entities, reranker flags), not the chunk's own metadata.
- The Go oracle's `SearchResult` has a `Metadata map[string]interface{}` field that the Rust port drops (the Rust `SearchResult` has no equivalent, and the MCP `ResultItem` has no `metadata` field).
- The `documents.metadata_json` column is stored as `Option<String>` (raw JSON), parsed on demand — the pattern to follow for `chunks.metadata_json`.
- `ChunkDao` uses explicit column lists (`SELECT_CHUNK`, `FTS_QUERY`) and a `row_to_chunk` mapper; `create_with_search_text` and `update` are the write paths.

## Goals / Non-Goals

**Goals:**
- Give each chunk its own free-form metadata bag (`DocumentChunk.metadata: Map<String, Value>`), not a clone of the document's typed struct.
- Persist the chunk's metadata bag to a `chunks.metadata_json` column (restoring the field from the original Rust design).
- Surface the section context in search: the result `text` is the pure `chunk_text`, and the chunk's metadata bag is carried on the result and the MCP response.
- Keep the legs matching on `search_text` (no change to retrieval), keep NER on the same keys, and keep the byte-offset invariant.

**Non-Goals:**
- Not changing chunk boundaries, `max_chunk_size`, or overlap.
- Not changing what the legs match on (FTS index and embeddings stay on `search_text`).
- Not touching the NER provider logic — only the call site changes.
- Not adding the Go oracle's document-level metadata keys to the wire `metadata` field (deferred).
- Not altering the ANN engine, RRF constants, rerank boosts, or the MCP/CLI surface.
- Not migrating or opening any legacy Go `knowledge.db`.

## Decisions

### D1 — New `metadata_json` column (the consolidated init migration)
Add `metadata_json TEXT` (nullable) to `chunks`, folded into the single squashed init migration `migrations/knowledge/1-init/up.sql`. It stores the chunk's metadata bag as raw JSON (`Option<String>`), following the `documents.metadata_json` pattern (raw text, parsed on demand — no custom `FromSql`).

- **Why a stored column vs. computing on the fly:** the bag is produced at ingestion and is stable for the lifetime of the chunk; the search layer needs it per hit without re-deriving it from the source. A column is the natural home, and it is the field the original Rust design already had.
- **Why nullable (not `NOT NULL DEFAULT '{}'`):** a chunk with no chunk-specific metadata (e.g. a fixed-span chunk with no section) legitimately has an empty bag; `NULL` reads as "no metadata" and avoids storing `'{}`' for the common case. The reader treats `NULL` and `""` as an empty bag.
- **Alternatives considered:** (a) a forward-only migration `2-chunk-metadata` — rejected (user decision S2): the DB is built from scratch pre-release, so editing the single squashed init is the honest home and keeps `user_version = 1`; (b) a second JSON column per key — rejected: the bag is a free-form map, a single JSON column is the oracle's own shape.

### D2 — `DocumentChunk.metadata` becomes a free-form bag
`DocumentChunk.metadata` changes from `DocumentMetadata` to `Map<String, Value>`. The bag holds the chunk's own metadata: the chunk-specific keys (`section_title`, `heading_level`, `breadcrumb`, `image_paths`) plus any document-level keys the chunker copies in (the current `extra` contents). The document's typed fields (`source_type`, `source_file`, `file_size`, `modified_at`) are **not** part of the chunk's bag — they stay on `Document.metadata` and are persisted to `documents.metadata_json`.

- **Why a bag vs. keeping `DocumentMetadata`:** a chunk is a chunking artifact; it should not own a clone of the document's typed struct (the modeling mistake). A free-form bag matches the oracle's `map[string]interface{}` and is the unit that NER consumes and that gets persisted.
- **Alternatives considered:** (a) keep `DocumentMetadata` and add a separate `chunk_meta: Map` field — rejected: it leaves the modeling mistake in place and doubles the surface; (b) a dedicated `ChunkMetadata` struct with typed fields — rejected (user decision): the keys are format-specific and open-ended; a free-form bag is the right shape and matches the oracle.

### D3 — The chunkers build the bag directly
Each chunker builds the chunk's `Map<String, Value>` bag directly, replacing the current "clone the document's `DocumentMetadata` and insert into `extra`" pattern. The bag carries the same keys the chunk's `extra` carried before (so NER and the persisted metadata see the same keys); the document's typed fields are no longer cloned into the chunk.

- **Why:** with `DocumentChunk.metadata` a bag, the chunker must produce a bag. Building it directly (rather than cloning a `DocumentMetadata` and discarding the typed fields) is the clean expression of D2.
- **Alternatives considered:** keep cloning a `DocumentMetadata` internally and only convert at the type boundary — rejected: it preserves the mistake and adds a conversion step.

### D4 — Result `text` = pure `chunk_text`; metadata carried on the result
`SearchResult.chunk_text` (and the hit `chunk_text` fields) become the chunk's pure `chunk_text` (the byte-offset slice), not `search_text`. The chunk's metadata bag is carried on the result (a `chunk_metadata: Map<String, Value>` field) and the MCP `ResultItem` exposes it as `metadata`. The legs still *match* on `search_text` (the FTS index and the embeddings are unchanged) — only the returned `text` and the new `metadata` field change.

- **Why:** the returned `text` should be the pure body (the byte-offset slice a caller can map back into the source); the section context belongs in structured `metadata`, not glued into the body. This matches the oracle's intent (the oracle's `text` is the body, its section context is in `Metadata`).
- **Why the legs keep matching on `search_text`:** retrieval quality depends on the legs seeing the breadcrumb (the whole point of the `search_text` change). Changing the match input would regress retrieval; only the *presentation* (returned text + metadata) changes.
- **Alternatives considered:** (a) keep `text` = `search_text` and add `metadata` — rejected (user decision): the body should be the pure slice; (b) drop `search_text` entirely and match on `chunk_text` — rejected: it would regress the lexical leg (heading terms) and the semantic leg (sectioned embeddings).

### D5 — The wire `metadata` carries the chunk's bag (raw), document-level keys deferred
The MCP `search` result item's `metadata` field is the chunk's own metadata bag, serialized raw (no curation). The Go oracle's *document-level* keys in `SearchResult.Metadata` (`document_source_type`, `updated_at`, `document_metadata_json`) are **not** included — they are a separate, deferred concern (user decision S4). The enrichment bag (domains, entities) is already exposed via the existing `domains`/`entities` fields and is untouched.

- **Why raw:** the bag is small and its keys are a known vocabulary; curating it on the wire adds a maintenance surface for no parity benefit. The caller can read the keys it needs.
- **Alternatives considered:** (a) merge the chunk bag into the enrichment bag and expose one `metadata` — rejected: it conflates two different data sources and risks key collisions; (b) expose only the section keys (breadcrumb/section_title) — rejected (user decision S3): the whole bag is the unit, and callers may want `image_paths`/`heading_level`.

### D6 — NER call site only
`NerProvider::extract_entities` already takes a `&Map<String, Value>`; the ingester's call site changes from `&chunk.metadata.extra` to `&chunk.metadata`. No provider (regex/llm/composite) logic changes, and the keys NER sees are the same (the bag carries the same keys the `extra` did).

- **Why:** D2 makes the bag the chunk's metadata; NER already consumes a bag, so this is a one-line call-site change with no behavior change.

## Risks / Trade-offs

- [Type-hygiene diff size] Changing `DocumentChunk.metadata` to a bag touches the three chunkers, the NER call site, and every `DocumentChunk { … }` construction site (≈13 across `worker.rs`, `runner/mod.rs`, tests). → Mitigation: it is isolated to `crates/ingestion`; the change is mechanical (bag instead of struct), and the acceptance criteria pin "same keys, same boundaries, same text". If the diff exceeds ~500 lines it is split (chunkers first, construction sites second).
- [FTS external-content vs. returned text] The FTS5 index is external-content over `search_text`, but the returned `text` reads `chunk_text`. → Mitigation: this is compatible (the index matches on `search_text`; the result reads a different column), and a targeted test pins it: a term present only in `search_text` matches, and the returned `text` is the pure `chunk_text`.
- [v5 schema deviation] The `chunks` table no longer matches the oracle v5 shape exactly (it gains `metadata_json`). → Mitigation: this is the single explicit, justified frozen-contract change (restores a field from the original Rust design; surfaces section context); the Rust DB is built from scratch (no legacy DB opened or migrated), and `user_version` stays 1.
- [Residual parity gap] The wire `metadata` (chunk bag) is a *subset* of the Go oracle's `SearchResult.Metadata` (which also has document-level keys, deferred per D5). → Mitigation: the parity harness compares only `{document_id, chunk_id}` + `total_count` (it strips `text`/`metadata`), so rank identity is unaffected; the document-level keys are a tracked, deferred follow-up, not a silent drop.

## Migration Plan

1. The `metadata_json` column is folded into the single consolidated init
   migration `migrations/knowledge/1-init/up.sql`: `chunks` gains
   `metadata_json TEXT` (nullable), alongside the existing `search_text`
   re-point. `rusqlite_migration` sets `PRAGMA user_version` to the migration
   count — one migration → `user_version = 1` (unchanged).
2. `crates/db`: `Chunk` gains `metadata_json: Option<String>`; `SELECT_CHUNK`,
   `FTS_QUERY`, `row_to_chunk`, and the write paths carry it.
3. `crates/ingestion`: `DocumentChunk.metadata` becomes a bag; the chunkers
   build it; the ingester serializes it to `metadata_json`; the NER call site
   reads the bag.
4. `crates/search`: hit/result `chunk_text` = pure `chunk_text`; the result
   carries the chunk's bag.
5. `crates/mcp`: the `search` result item gains a `metadata` field.
6. Re-pin `crates/parity-harness/fixtures/content/search.json` (`text` = pure
   body); confirm the rank-order parity is unchanged.

**Rollback:** none (the DB is built from scratch, so there is no rollback target).

## Open Questions

- None. The scope is fixed: `text` = pure `chunk_text`, `metadata_json` restored
  and surfaced, `DocumentChunk.metadata` a free-form bag, document-level wire
  keys deferred. The data-schema deviation is the one explicit frozen-contract
  decision (user-authorized).
