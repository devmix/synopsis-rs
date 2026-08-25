# Proposal: search-module

## Change name
`search-module`

## Why

The ingestion series delivered the knowledge base; nothing can query it yet.
The oracle's search layer (`../synopsis/internal/search/`, ~1330 lines + tests)
provides lexical (FTS5/BM25), semantic (vector cosine), and hybrid search fused
by Reciprocal Rank Fusion, then enriched/reranked/graph-expanded — it backs the
`search`/`hybrid_search` MCP tools that the mcp change will expose. This change
ports that layer as a new `crates/search` per the workspace layout.

## What changes

1. **Core types + RRF fusion** — `SearchResult`, raw lexical/semantic hit types,
   and the ReciprocalRankFusion port: score accumulation over both lists,
   min-max BM25 normalization (lexical-only range, neutral 0.5 for semantic-only),
   RRF normalization, 0.7·rrf + 0.3·bm25 calibration, deterministic chunk_id
   tiebreak. Full parity with `rrf_test.go`.
2. **Sub-searchers** — lexical over `ChunkDao::search_fts` (already fixed at the
   DAO level to actually ORDER BY bm25 — an oracle bug recorded during the db
   change); semantic: embed query → vectors engine `search()` → resolve chunk
   rows → application-side domain filter with over-fetch (design D3).
3. **Enricher** — batch document metadata + per-chunk entities, source-type
   merging, updated_at RFC3339 normalization, reranker flag extraction, domain
   normalization. Needs a small batch `get_entities_by_chunks` addition to the
   db crate.
4. **Reranker** — business rules (deprecated ×0.2, official ×1.5, expired ×0.1),
   freshness boost (recent_days), authority boost map, re-sort + rank reassign.
5. **Graph expander** — BFS expansion of result entities via the graph crate,
   batch approved facts, serialize edges/facts into `related_entities` metadata;
   failures are non-fatal by contract.
6. **HybridSearcher** — orchestration: both sub-searches → both-fail error /
   one-fail degrade → RRF fuse → finalize (enrich → rerank → truncate topK →
   optional expand).
7. **Integration test** — in-memory SQLite + real FTS5 + mock embeddings +
   in-memory vector index through the public API.

## Non-goals

- No MCP tools (the mcp change consumes this crate).
- No query caching / result caching layers.
- Parallel sub-search execution is deliberately dropped (design D2): both legs
  are local millisecond-scale operations; goroutine+timeout machinery buys
  nothing on a laptop and complicates the sync design.
- No reranker ML model — the oracle's rule-based reranker is ported as-is.

## Risks

- Semantic domain filtering moves from SQL to application side (design D3) —
  over-fetch factor must keep result quality; pinned by tests.
- RRF calibration numbers (k=20, 0.7/0.3) are behavioral parity targets —
  differential tests against oracle values.
