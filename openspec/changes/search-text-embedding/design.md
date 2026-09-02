## Context

See proposal.md for the motivation. The current state that shapes the approach:

- `DocumentChunk.text` is a **pure source slice** (byte-offset invariant: `content[start_offset..end_offset] == text`); the heading breadcrumb lives only in `metadata.extra["breadcrumb"]` (`crates/ingestion/src/chunkers/markdown.rs`).
- The ingester computes the embedding on `chunk.text` and writes `chunk.text` to the `chunks.chunk_text` column (`crates/ingestion/src/ingester/mod.rs`).
- The FTS5 index `chunks_fts` is an **external-content** table (`content='chunks'`) that indexes `chunk_text`, kept in sync by the `chunks_fts_ai/ad/au` triggers (`migrations/knowledge/1-init/up.sql`).
- The search result's `text` field is `chunk_text` (`crates/search/src/lib.rs` `SearchResult`).
- NER runs on `chunk.text` + `metadata.extra`, with the breadcrumb substituted in the prompt template (`crates/ingestion/src/ingester/mod.rs`).
- The Go oracle feeds **both** search legs the breadcrumb-prefixed text, so its search identity diverges from Rust's (surfaced by `parity-fixture-expansion` task 1.5).

## Goals / Non-Goals

**Goals:**
- Give the lexical and semantic legs the section context (breadcrumb) via a per-chunk `search_text = breadcrumb + "\n\n" + body`.
- Restore search identity parity with the oracle without weakening the parity `normalize`.
- Preserve the byte-offset invariant on `chunk_text` and leave NER unchanged.

**Non-Goals:**
- Not changing chunk boundaries (section splitting, overlap, `max_chunk_size`).
- Not altering RRF constants, rerank boosts, the ANN engine, or the MCP/CLI surface.
- Not migrating or opening any legacy Go `knowledge.db`.

## Decisions

### D1 — New `search_text` column (the consolidated init migration)
Add `search_text TEXT NOT NULL` (default = `chunk_text`) to `chunks`. The Markdown chunker builds it as `breadcrumb + "\n\n" + body`, or `body` when there is no breadcrumb. The column (and the FTS re-point, D2) is folded into the single squashed init migration `migrations/knowledge/1-init/up.sql` (task 5.1 consolidated the former five forward-only migrations into one).

- **Why a stored column vs. computing on the fly:** the FTS5 index is an external-content table that indexes a *column* of `chunks`; re-pointing it to `search_text` requires the column to exist. The embedding (computed at ingestion) and the result `text` field also need it, so storing it once is the natural home.
- **Alternatives considered:** (a) compute `search_text` on the fly in the search pipeline only — rejected, the FTS index cannot index a non-column and the embedding is computed at ingestion; (b) overwrite `chunk_text` with the breadcrumb-prefixed text (the oracle's approach) — rejected, it breaks the byte-offset invariant, which is a deliberate Rust correctness fix.

### D2 — Re-point the FTS5 index to `search_text`
Drop and re-create `chunks_fts` to index `search_text`; update the `ai/ad/au` triggers to reference `search_text`.

- **Why:** the lexical leg must see heading terms (this is what fixes the chunk-25 "atlas" miss — "atlas" appears in that chunk only via its breadcrumb). User-confirmed: both legs get the breadcrumb, not just the embedding leg.
- **Alternatives considered:** keep FTS on `chunk_text` and only give the embedding leg the breadcrumb — rejected (user decision): it would fix only half the divergence and leave the lexical leg missing heading terms.

### D3 — Embedding computed on `search_text`
The ingester passes `chunk.search_text` to the embedding provider (instead of `chunk.text`) and persists both `chunk_text` and `search_text`.

- **Why:** the semantic leg must embed the section-context text so its vectors match the oracle's. The embedding is computed at ingestion time, so this is a one-line input change in the ingester.

### D4 — Result `text` field = `search_text`
`SearchResult.text` (and the lexical/semantic hit `chunk_text` fields that feed it) become the chunk's `search_text`.

- **Why:** a returned chunk should carry its section context for the LLM/user, matching the oracle's result shape. The byte-offset invariant is unaffected (it is on `chunk_text`, which is still stored and returned for mapping).

### D5 — NER unchanged
Entity extraction continues to run on `chunk.text` + `metadata.extra` (breadcrumb substituted in the prompt template as today).

- **Why:** per the task owner, NER already substitutes the breadcrumb in its template; it does not need the embedding/FTS change. Keeping NER on `text` also keeps its parity surface stable.

## Risks / Trade-offs

- [Embedding context length] The breadcrumb prepends tokens to the embedded text, using part of the model's context window. → Mitigation: the breadcrumb is short (a few heading lines) and the body remains the dominant signal; bge-small (384-dim) and bge-m3 (1024-dim) both have ample context for the chunk sizes used.
- [v5 schema deviation] The `chunks` table no longer matches the oracle v5 shape exactly. → Mitigation: this is the single explicit, justified frozen-contract change (retrieval quality); the Rust DB is always built from scratch (no legacy DB is opened or migrated), and the byte-offset invariant is preserved on `chunk_text`.
- [FTS re-create] Dropping + re-creating the external-content FTS index is a structural change. → Mitigation: forward-only migration; built-from-scratch DB means no data loss; the triggers are updated atomically in the same migration.
- [Residual parity gap] Even with identical leg inputs, a small identity difference could remain (e.g. FTS5 tokenizer vs the oracle's tokenizer, or embedding float rounding at the margin). → Mitigation: `parity-fixture-expansion` task 1.5 re-runs after this change; any residual difference is surfaced and investigated rather than papered over by weakening `normalize`.

## Migration Plan

1. The `search_text` re-point is folded into the single consolidated init
   migration `migrations/knowledge/1-init/up.sql` (task 5.1 squashed the former
   five forward-only migrations into one): `chunks` carries `search_text TEXT
   NOT NULL DEFAULT ''` (default = `chunk_text`), `chunks_fts` is created
   indexing `search_text` (`content='chunks'`, `content_rowid='id'`), and the
   `chunks_fts_ai/ad/au` triggers reference `search_text`. `rusqlite_migration`
   sets `PRAGMA user_version` to the migration count — one migration →
   `user_version = 1` (human decision 2026-09-02, option B: re-number to 1; the
   schema SHAPE is still the full v5 shape).
2. `crates/db`: `Chunk` + `ChunkDao` + FTS query/`SELECT_CHUNK` updated to the new column.
3. `crates/ingestion`: chunker emits `search_text`; ingester embeds + persists it.
4. `crates/search`: result `text` = `search_text`.
5. Re-run `parity-fixture-expansion` task 1.5 to confirm full identity parity.

**Rollback:** none (the DB is built from scratch, so there is no rollback target).

## Open Questions

- None. The scope is fixed: FTS-on-`search_text` (both legs) and NER-unchanged were confirmed by the task owner; the data-schema deviation is the one explicit frozen-contract decision.
