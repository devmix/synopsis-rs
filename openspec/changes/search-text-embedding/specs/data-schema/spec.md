## ADDED Requirements

### Requirement: search_text column (explicit v5 deviation)
The `chunks` table carries a `search_text TEXT NOT NULL` column (default = `chunk_text`) holding the search-oriented text: for Markdown chunks the heading breadcrumb (multi-line heading path) followed by the chunk body, or the body alone when the chunk has no breadcrumb. The FTS5 `chunks_fts` index is built over `search_text` (not `chunk_text`), and its `ai/ad/au` triggers reference `search_text`. This is an explicit, justified deviation from the oracle v5 shape: the Rust database is always built from scratch (no legacy `knowledge.db` is opened or migrated), and the deviation improves RAG retrieval quality by giving both search legs the section context. The invariant-preserving `chunk_text` column and the byte offsets are unchanged.

#### Scenario: Fresh build includes search_text
- **WHEN** a fresh knowledge database is built from the consolidated init migration
- **THEN** the `chunks` table has a `search_text` column, the `chunks_fts` index indexes `search_text`, and `PRAGMA user_version` = 1

#### Scenario: FTS index tracks search_text
- **WHEN** a chunk is inserted, updated, or deleted
- **THEN** the `chunks_fts` index is kept in sync via the `ai`/`au`/`ad` triggers over `search_text`

#### Scenario: chunk_text invariant preserved
- **WHEN** a chunk is stored
- **THEN** `chunk_text` remains the pure source slice and `start_offset`/`end_offset` still satisfy `content[start_offset..end_offset] == chunk_text`
