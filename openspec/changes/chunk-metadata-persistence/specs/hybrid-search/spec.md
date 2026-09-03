## MODIFIED Requirements

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
