## ADDED Requirements

### Requirement: Embedding and persistence of search_text
The per-document ingestion pipeline computes each chunk's embedding from `search_text` (not `text`) and persists both `chunk_text` (the invariant-preserving body) and `search_text` to the `chunks` table. Entity extraction (NER) is unchanged: it continues to run on `text` plus the metadata extras, with the breadcrumb substituted in the NER prompt template exactly as before.

#### Scenario: Embedding input is search_text
- **WHEN** the pipeline embeds a batch of chunks
- **THEN** the embedding provider receives each chunk's `search_text` (not its `text`)

#### Scenario: Both fields persisted
- **WHEN** the pipeline writes a chunk to the database
- **THEN** both `chunk_text` and `search_text` are stored on the chunk row

#### Scenario: NER input unchanged
- **WHEN** the pipeline runs entity extraction on a chunk
- **THEN** NER receives `text` and the metadata extras (not `search_text`), as before this change
