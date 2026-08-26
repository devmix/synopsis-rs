# Design: search-module

Oracle references (read-only): `../synopsis/internal/search/*.go` (+ tests),
`crates/db/src/chunk.rs` (search_fts already delivered with the ORDER BY fix).

## D1 — New crate `crates/search`, modules mirror the oracle seams

```
crates/search/src/
  lib.rs        — SearchResult, Searcher trait, SearcherBuilder/new
  rrf.rs        — ReciprocalRankFusion + normalizations
  lexical.rs    — FTS5 sub-searcher over ChunkDao::search_fts
  semantic.rs   — vector sub-searcher over EmbeddingProvider + VectorIndex
  enrich.rs     — Enricher
  rerank.rs     — Reranker
  expand.rs     — GraphExpander
  hybrid.rs     — HybridSearcher orchestration + finalize pipeline
```

Dependency edges per D1: search → {config, db, embedding, vectors, graph}.
Scaffolding follows the ingestion change precedent (workspace member, thiserror
SearchError, missing_docs deny).

## D2 — Sequential sub-searches; timeout dropped (deviation)

The oracle runs both legs in goroutines with a context timeout. Our stack is
synchronous and both legs are local millisecond-scale operations (FTS5 query,
lancedb ANN lookup). HybridSearch runs them sequentially; `timeout_ms` stays in
SearchConfig (frozen config format) but is not plumbed — recorded deviation.
Rationale: no concurrency to manage, deterministic tests, no behavioral
difference a user can observe at these latencies.

## D3 — Semantic domain filtering: application-side with over-fetch (deviation)

The oracle's vec0 SearchVector filtered by domain inside SQL (vec0 tables lived
next to chunks). Our vectors engine is lancedb-backed and domain-blind. The
semantic leg fetches `topK × OVERFETCH_FACTOR` (constant 3, module-documented)
hits from the index, resolves chunk rows + document domains via DAO batch
queries, filters by normalized domain, then truncates to topK. Lexical domain
filtering stays SQL-side (`search_fts` already takes `Option<&str>`).
Pinned by tests: filter correctness and that enough results survive.

## D4 — RRF port: faithful numbers, documented quirks

- k defaults to 20; score += 1/(k + rank) per list (1-based ranks);
- BM25 min-max normalization computed ONLY over entries present in lexical
  results (semantic-only entries get neutral 0.5) — preserves the oracle's
  mixed-pool behavior;
- RRF scores min-max normalized to [0,1]; final = 0.7·rrf_norm + 0.3·bm25_norm;
- sort: descending score, ascending chunk_id tiebreak; topN ≤ 0 → no truncation;
- source_type: "lexical" | "semantic" | "hybrid" (both lists).

Differential parity against `rrf_test.go` values is an acceptance criterion.

## D5 — Finalize pipeline order (contract)

enrich → rerank → truncate to topK → graph expand (non-fatal). Domain filtering
happens inside the sub-searches (lexical SQL-side, semantic application-side),
never after fusion — matches the oracle contract so up to topK results survive
within the requested domain.

## D6 — Enricher details

Batch `DocumentDao::get_by_ids` + new batch `ChunkEntityDao::get_entities_by_chunks`
(db-crate addition, IN-list batching like existing DAOs). Per result:
document_path, source_type merge ("lexical+pdf" style), updated_at normalized to
RFC3339 (accepts RFC3339 / SQLite CURRENT_TIMESTAMP layout / fractional), reranker
flags (is_deprecated/is_official/valid_to) copied from document metadata JSON,
domains extracted as Vec<String>. Entities attached from the batch map.

## D7 — Reranker port

Boost factors default deprecated=0.2, official=1.5, recent=1.2, recent_days=90;
config overrides only when > 0; authority_boost map keyed by document source
type. Business rules multiply factors (deprecated × official compose); expired
(valid_to < now) ×0.1. After boosts: re-sort descending, reassign 1-based ranks.
Freshness parses RFC3339 from enriched metadata.

## D8 — Graph expander

Enabled when graph config enables it AND a Graph handle is provided (injected;
SetGraph-style swap is unnecessary for us — the CLI rebuilds the searcher).
For each result entity present in the graph: BFS both-directions via the graph
crate traverser (max_depth/max_nodes from GraphConfig), batch approved facts via
FactDao::list_by_entity_ids, serialize edges (source/target/relation/method/
confidence/evidence) and facts into metadata["related_entities"]. Expansion
errors are warnings — results return without graph context (oracle contract).

## D9 — Errors

`SearchError` (thiserror): both-subsearches-failed carries both causes; DAO /
embedding / vector errors wrap their crate errors. Empty query → Ok(empty vec)
(oracle returns nil, nil).
