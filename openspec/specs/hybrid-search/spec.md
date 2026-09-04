# hybrid-search Specification

## Purpose

Hybrid knowledge search: the lexical leg (FTS5/BM25), the semantic leg (vector similarity), Reciprocal Rank Fusion with BM25 calibration, enrichment, business-rule reranking, and graph expansion.

## Requirements

### Requirement: Hybrid search

The search crate provides the trait `Searcher` with three methods: `hybrid_search` (both sub-searches → RRF fusion), `lexical_search` (FTS5/BM25), `semantic_search` (vector similarity). An empty query returns an empty result; on the failure of both sub-searches — an error with both reasons; on the failure of one — processing continues with the surviving results. The final pipeline (enrich → rerank → truncate → expand) runs on all paths, including the single legs.

#### Scenario: One sub-search failure
- **WHEN** the lexical search failed and the semantic one succeeded
- **THEN** the semantic search results are returned without an error

#### Scenario: Both sub-searches failed
- **WHEN** both sub-searches returned errors
- **THEN** the hybrid search fails, mentioning both reasons

### Requirement: Reciprocal Rank Fusion

The fusion merges two ranked lists: score += 1/(k + rank) per list (k default 20); BM25 scores are min-max normalized only over the lexical list's entries (semantically-unique ones get a neutral 0.5); RRF scores are normalized into [0,1]; the final is 0.7·rrf + 0.3·bm25; sorted in descending score order with a deterministic tiebreak by ascending chunk_id; result type: lexical | semantic | hybrid.

#### Scenario: Chunk in both lists
- **WHEN** a chunk is found by both the lexical and the semantic search
- **THEN** its RRF score is summed from both lists and the type is marked hybrid

#### Scenario: Deterministic order
- **WHEN** two chunks have an equal final score
- **THEN** the chunk with the smaller chunk_id ranks higher

### Requirement: Domain filtering

Domain filtering is performed inside the sub-searches (lexical — at the SQL level, semantic — on the application side with ×3 over-fetching), before the fusion and truncation: up to topK results within the requested domain survive. Domain comparison is normalized (case/whitespace).

#### Scenario: Domain filter before truncation
- **WHEN** hybrid_search is called with a domain and a topK
- **THEN** up to topK results from that domain are returned, not fewer due to post-filtering

### Requirement: Result enrichment

The enricher adds to the results in batches: the document path, the merged type (search+document), updated_at in RFC3339 (accepts RFC3339 and the SQLite CURRENT_TIMESTAMP format), the reranker flags (is_deprecated/is_official/valid_to) from the document metadata, and the domain list; the chunk's entities are attached in a single batch query.

#### Scenario: Batch enrichment
- **WHEN** the enriched pool contains results from several documents
- **THEN** the documents and entities are requested in batches, without N+1

### Requirement: Reranking

The reranker applies business rules (deprecated ×0.2, official ×1.5, expired valid_to ×0.1 — the multipliers compose), a freshness boost for documents updated within recent_days (×recent_boost), and an authority boost by document type from the configuration map; then it re-sorts in descending score order and re-numbers the ranks. Default values: 0.2/1.5/1.2/90; the config overrides only positive values.

#### Scenario: Boost composition
- **WHEN** a document is both deprecated and official
- **THEN** the score is multiplied by both factors (0.2 × 1.5)

#### Scenario: Rank renumbering
- **WHEN** the boosts change the order of the results
- **THEN** the ranks are reassigned according to the new order after the topK truncation

### Requirement: Graph expansion

If enabled by the configuration and the graph is provided: the results' entities are expanded by BFS in both directions (max_depth/max_nodes), approved facts are loaded in batches; edges and facts are serialized into metadata.related_entities. Expansion errors are not fatal — the results are returned without graph context.

#### Scenario: Non-critical expansion failure
- **WHEN** the graph traversal fails
- **THEN** the search returns the enriched results without related_entities, without an error

### Requirement: Search parity

The RRF fusion, the reranker, and the enrichment are verified against recorded fixtures: the calibration constants (k=20, 0.7/0.3), the normalizations, the boosts, and the order yield the same values as fixed in the recorded cases; end-to-end scenarios are run through real FTS5 with manually computed expectations.

#### Scenario: Recorded-case run
- **WHEN** the cases (rrf, enricher, reranker, graph expansion) are run through the Rust implementation
- **THEN** the scores, orders, and metadata match the recorded fixtures

### Requirement: Search legs consume search_text
Both search legs SHALL be fed the chunk's `search_text` for **matching**: the lexical leg matches the FTS5 index (built over `search_text`) and the semantic leg compares the query embedding against chunk embeddings computed from `search_text`. The fused, ranked result's `text` field SHALL be the chunk's pure `chunk_text` (the byte-offset slice), and the chunk's metadata bag (`section_title`, `heading_level`, `breadcrumb`, `image_paths`, …) SHALL be carried on the result as structured `metadata` — the section context that was previously glued into the text. The fusion (RRF), reranking, and enrichment are otherwise unchanged.

#### Scenario: Lexical leg sees heading terms
- **WHEN** a query term appears in a chunk's heading breadcrumb but not in its body
- **THEN** the lexical leg can match that chunk (the FTS index is over `search_text`)

#### Scenario: Semantic leg embeds sectioned text
- **WHEN** the semantic leg ranks chunks for a query
- **THEN** it compares against embeddings computed from `search_text` (breadcrumb + body)

#### Scenario: Result text carries section context
- **WHEN** a chunk is returned in a search result
- **THEN** the result's `text` field is the chunk's pure `chunk_text` (the byte-offset slice), and the section context (breadcrumb, section title) is carried in the result's `metadata` field, not glued into the text

#### Scenario: Result carries the chunk metadata
- **WHEN** a chunk is returned in a search result
- **THEN** the result's `metadata` carries the chunk's own metadata bag (`section_title`, `heading_level`, `breadcrumb`, …); a chunk with no metadata carries an empty bag
