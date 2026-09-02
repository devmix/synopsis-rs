## ADDED Requirements

### Requirement: ChunkDao and FTS over search_text
The `Chunk` row type exposes both `chunk_text` and `search_text`. `ChunkDao::create` and `ChunkDao::update` accept a `search_text` value and store it alongside `chunk_text`. The FTS5 index operates on `search_text`, so full-text matches are made against the section-context text, while row reads still return `chunk_text` as the chunk body and the byte offsets are unchanged.

#### Scenario: Create stores both fields
- **WHEN** a chunk is created with a `chunk_text` and a `search_text`
- **THEN** both are stored and a subsequent read returns both values

#### Scenario: FTS matches on search_text
- **WHEN** a full-text query matches a term that appears only in a chunk's `search_text` (e.g. a heading term absent from `chunk_text`)
- **THEN** that chunk is returned by the FTS search

#### Scenario: Row read returns chunk_text
- **WHEN** a chunk row is read
- **THEN** the returned body is `chunk_text` (the pure source slice), with `search_text` available as a separate field
