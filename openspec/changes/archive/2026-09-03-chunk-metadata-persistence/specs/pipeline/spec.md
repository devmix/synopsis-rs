## MODIFIED Requirements

### Requirement: Embedding and persistence of search_text
The per-document ingestion pipeline SHALL compute each chunk's embedding from `search_text` (not `text`) and SHALL persist both `chunk_text` (the invariant-preserving body) and `search_text` to the `chunks` table. It SHALL also serialize each chunk's own metadata bag (`section_title`, `heading_level`, `breadcrumb`, `image_paths`, …) to the `chunks.metadata_json` column (a chunk with an empty bag stores `NULL`); the document-level `extra` bag is still serialized to `documents.metadata_json` as before. Entity extraction (NER) is unchanged: it SHALL receive the chunk's metadata bag (the same keys as before) and run on `text`.

#### Scenario: Embedding input is search_text
- **WHEN** the pipeline embeds a batch of chunks
- **THEN** the embedding provider receives each chunk's `search_text` (not its `text`)

#### Scenario: Both fields persisted
- **WHEN** the pipeline writes a chunk to the database
- **THEN** both `chunk_text` and `search_text` are stored, and the chunk's own metadata bag is stored in `metadata_json` (a chunk with no chunk-specific metadata stores `NULL`)

#### Scenario: NER input unchanged
- **WHEN** the pipeline runs entity extraction on a chunk
- **THEN** NER receives `text` and the chunk's metadata bag (the same keys it received before this change), not `search_text`
