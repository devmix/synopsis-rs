## ADDED Requirements

### Requirement: Search legs consume search_text
Both search legs are fed the chunk's `search_text` rather than `chunk_text`: the lexical leg matches the FTS5 index (built over `search_text`) and the semantic leg compares the query embedding against chunk embeddings computed from `search_text`. The fused, ranked result's `text` field is `search_text`, so a returned chunk carries its section context. The fusion (RRF), reranking, enrichment, and the response field set are otherwise unchanged.

#### Scenario: Lexical leg sees heading terms
- **WHEN** a query term appears in a chunk's heading breadcrumb but not in its body
- **THEN** the lexical leg can match that chunk (the FTS index is over `search_text`)

#### Scenario: Semantic leg embeds sectioned text
- **WHEN** the semantic leg ranks chunks for a query
- **THEN** it compares against embeddings computed from `search_text` (breadcrumb + body)

#### Scenario: Result text carries section context
- **WHEN** a chunk is returned in a search result
- **THEN** the result's `text` field is the chunk's `search_text` (breadcrumb + body)
